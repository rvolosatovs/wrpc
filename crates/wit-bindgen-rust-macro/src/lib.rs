use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use syn::parse::{Error, Parse, ParseStream, Result};
use syn::punctuated::Punctuated;
use syn::{braced, token, Token};
use wit_bindgen_core::wit_parser::{PackageId, Resolve, UnresolvedPackageGroup, WorldId};
use wit_bindgen_wrpc_rust::{Opts, WithOption};

#[proc_macro]
pub fn generate(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    syn::parse_macro_input!(input as Config)
        .expand()
        .unwrap_or_else(Error::into_compile_error)
        .into()
}

fn anyhow_to_syn(span: Span, err: anyhow::Error) -> Error {
    let mut msg = err.to_string();
    for cause in err.chain().skip(1) {
        msg.push_str(&format!("\n\nCaused by:\n  {cause}"));
    }
    Error::new(span, msg)
}

struct Config {
    opts: Opts,
    resolve: Resolve,
    world: WorldId,
    files: Vec<PathBuf>,
}

/// The source of the wit package definition
enum Source {
    /// A path to a wit directory
    Path(String),
    /// Inline sources have an optional path to a directory of their dependencies
    Inline(String, Option<PathBuf>),
}

impl Parse for Config {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let call_site = Span::call_site();
        let mut opts = Opts::default();
        let mut world = None;
        let mut source = None;
        let mut features = Vec::new();

        if input.peek(token::Brace) {
            let content;
            syn::braced!(content in input);
            let fields = Punctuated::<Opt, Token![,]>::parse_terminated(&content)?;
            for field in fields.into_pairs() {
                match field.into_value() {
                    Opt::Path(s) => {
                        source = Some(match source {
                            Some(Source::Path(_) | Source::Inline(_, Some(_))) => {
                                return Err(Error::new(s.span(), "cannot specify second source"));
                            }
                            Some(Source::Inline(i, None)) => {
                                Source::Inline(i, Some(PathBuf::from(s.value())))
                            }
                            None => Source::Path(s.value()),
                        });
                    }
                    Opt::World(s) => {
                        if world.is_some() {
                            return Err(Error::new(s.span(), "cannot specify second world"));
                        }
                        world = Some(s.value());
                    }
                    Opt::Inline(s) => {
                        source = Some(match source {
                            Some(Source::Inline(_, _)) => {
                                return Err(Error::new(s.span(), "cannot specify second source"));
                            }
                            Some(Source::Path(p)) => {
                                Source::Inline(s.value(), Some(PathBuf::from(p)))
                            }
                            None => Source::Inline(s.value(), None),
                        });
                    }
                    Opt::Skip(list) => opts.skip.extend(list.iter().map(syn::LitStr::value)),
                    Opt::AdditionalDerives(paths) => {
                        opts.additional_derive_attributes = paths
                            .into_iter()
                            .map(|p| p.into_token_stream().to_string())
                            .collect();
                    }
                    Opt::With(with) => opts.with.extend(with),
                    Opt::GenerateAll => {
                        opts.with.generate_by_default = true;
                    }
                    Opt::GenerateUnusedTypes(enable) => {
                        opts.generate_unused_types = enable.value();
                    }
                    Opt::AnyhowPath(path) => {
                        opts.anyhow_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::BitflagsPath(path) => {
                        opts.bitflags_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::BytesPath(path) => {
                        opts.bytes_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::FuturesPath(path) => {
                        opts.futures_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::TokioPath(path) => {
                        opts.tokio_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::TracingPath(path) => {
                        opts.tracing_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::WasmTokioPath(path) => {
                        opts.wasm_tokio_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::WrpcTransportPath(path) => {
                        opts.wrpc_transport_path = Some(path.into_token_stream().to_string());
                    }
                    Opt::Features(f) => {
                        features.extend(f.into_iter().map(|f| f.value()));
                    }
                }
            }
        } else {
            world = input.parse::<Option<syn::LitStr>>()?.map(|s| s.value());
            if input.parse::<Option<syn::token::In>>()?.is_some() {
                source = Some(Source::Path(input.parse::<syn::LitStr>()?.value()));
            }
        }
        let (resolve, pkgs, files) =
            parse_source(&source, &features).map_err(|err| anyhow_to_syn(call_site, err))?;
        let world = resolve
            .select_world(&pkgs, world.as_deref())
            .map_err(|e| anyhow_to_syn(call_site, e))?;
        Ok(Config {
            opts,
            resolve,
            world,
            files,
        })
    }
}

/// Parse the source
fn parse_source(
    source: &Option<Source>,
    features: &[String],
) -> anyhow::Result<(Resolve, Vec<PackageId>, Vec<PathBuf>)> {
    let mut resolve = Resolve::default();
    resolve.features.extend(features.iter().cloned());
    let mut files = Vec::new();
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let mut parse = |path: &Path| -> anyhow::Result<_> {
        // Try to normalize the path to make the error message more understandable when
        // the path is not correct. Fallback to the original path if normalization fails
        // (probably return an error somewhere else).
        let normalized_path = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => path.to_path_buf(),
        };
        let (pkg, sources) = resolve.push_path(normalized_path)?;
        files.extend(sources);
        Ok(pkg)
    };
    let pkg = match source {
        Some(Source::Inline(s, path)) => {
            if let Some(p) = path {
                parse(&root.join(p))?;
            }
            resolve.push_group(UnresolvedPackageGroup::parse("macro-input", s)?)?
        }
        Some(Source::Path(s)) => parse(&root.join(s))?,
        None => parse(&root.join("wit"))?,
    };

    Ok((resolve, pkg, files))
}

impl Config {
    fn expand(self) -> Result<TokenStream> {
        let mut files = Default::default();
        let mut generator = self.opts.build();
        generator
            .generate(&self.resolve, self.world, &mut files)
            .map_err(|e| Error::new(Span::call_site(), e))?;
        let (_, src) = files.iter().next().unwrap();
        let mut src = std::str::from_utf8(src).unwrap().to_string();

        // If a magical `WIT_BINDGEN_DEBUG` environment variable is set then
        // place a formatted version of the expanded code into a file. This file
        // will then show up in rustc error messages for any codegen issues and can
        // be inspected manually.
        if std::env::var("WIT_BINDGEN_DEBUG").is_ok() {
            static INVOCATION: AtomicUsize = AtomicUsize::new(0);
            let root = Path::new(env!("DEBUG_OUTPUT_DIR"));
            let world_name = &self.resolve.worlds[self.world].name;
            let n = INVOCATION.fetch_add(1, Relaxed);
            let path = root.join(format!("{world_name}{n}.rs"));

            // optimistically format the code but don't require success
            let contents = match fmt(&src) {
                Ok(formatted) => formatted,
                Err(_) => src.clone(),
            };
            std::fs::write(&path, contents.as_bytes()).unwrap();

            src = format!("include!({path:?});");
        }
        let mut contents = src.parse::<TokenStream>().unwrap();

        // Include a dummy `include_bytes!` for any files we read so rustc knows that
        // we depend on the contents of those files.
        for file in &self.files {
            contents.extend(
                format!(
                    "const _: &[u8] = include_bytes!(r#\"{}\"#);\n",
                    file.display()
                )
                .parse::<TokenStream>()
                .unwrap(),
            );
        }

        Ok(contents)
    }
}

mod kw {
    syn::custom_keyword!(skip);
    syn::custom_keyword!(world);
    syn::custom_keyword!(path);
    syn::custom_keyword!(inline);
    syn::custom_keyword!(exports);
    syn::custom_keyword!(additional_derives);
    syn::custom_keyword!(with);
    syn::custom_keyword!(generate_all);
    syn::custom_keyword!(generate_unused_types);
    syn::custom_keyword!(anyhow_path);
    syn::custom_keyword!(bitflags_path);
    syn::custom_keyword!(bytes_path);
    syn::custom_keyword!(futures_path);
    syn::custom_keyword!(tokio_path);
    syn::custom_keyword!(tracing_path);
    syn::custom_keyword!(wasm_tokio_path);
    syn::custom_keyword!(wrpc_transport_path);
    syn::custom_keyword!(features);
}

enum Opt {
    World(syn::LitStr),
    Path(syn::LitStr),
    Inline(syn::LitStr),
    Skip(Vec<syn::LitStr>),
    // Parse as paths so we can take the concrete types/macro names rather than raw strings
    AdditionalDerives(Vec<syn::Path>),
    With(HashMap<String, WithOption>),
    GenerateAll,
    GenerateUnusedTypes(syn::LitBool),
    AnyhowPath(syn::Path),
    BitflagsPath(syn::Path),
    BytesPath(syn::Path),
    FuturesPath(syn::Path),
    TokioPath(syn::Path),
    TracingPath(syn::Path),
    WasmTokioPath(syn::Path),
    WrpcTransportPath(syn::Path),
    Features(Vec<syn::LitStr>),
}

impl Parse for Opt {
    fn parse(input: ParseStream<'_>) -> Result<Self> {
        let l = input.lookahead1();
        if l.peek(kw::path) {
            input.parse::<kw::path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::Path(input.parse()?))
        } else if l.peek(kw::inline) {
            input.parse::<kw::inline>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::Inline(input.parse()?))
        } else if l.peek(kw::world) {
            input.parse::<kw::world>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::World(input.parse()?))
        } else if l.peek(kw::skip) {
            input.parse::<kw::skip>()?;
            input.parse::<Token![:]>()?;
            let contents;
            syn::bracketed!(contents in input);
            let list = Punctuated::<_, Token![,]>::parse_terminated(&contents)?;
            Ok(Opt::Skip(list.iter().cloned().collect()))
        } else if l.peek(kw::additional_derives) {
            input.parse::<kw::additional_derives>()?;
            input.parse::<Token![:]>()?;
            let contents;
            syn::bracketed!(contents in input);
            let list = Punctuated::<_, Token![,]>::parse_terminated(&contents)?;
            Ok(Opt::AdditionalDerives(list.iter().cloned().collect()))
        } else if l.peek(kw::with) {
            input.parse::<kw::with>()?;
            input.parse::<Token![:]>()?;
            let contents;
            let _lbrace = braced!(contents in input);
            let fields: Punctuated<_, Token![,]> =
                contents.parse_terminated(with_field_parse, Token![,])?;
            Ok(Opt::With(HashMap::from_iter(fields)))
        } else if l.peek(kw::generate_all) {
            input.parse::<kw::generate_all>()?;
            Ok(Opt::GenerateAll)
        } else if l.peek(kw::generate_unused_types) {
            input.parse::<kw::generate_unused_types>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::GenerateUnusedTypes(input.parse()?))
        } else if l.peek(kw::anyhow_path) {
            input.parse::<kw::anyhow_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::AnyhowPath(input.parse()?))
        } else if l.peek(kw::bitflags_path) {
            input.parse::<kw::bitflags_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::BitflagsPath(input.parse()?))
        } else if l.peek(kw::bytes_path) {
            input.parse::<kw::bytes_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::BytesPath(input.parse()?))
        } else if l.peek(kw::futures_path) {
            input.parse::<kw::futures_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::FuturesPath(input.parse()?))
        } else if l.peek(kw::tokio_path) {
            input.parse::<kw::tokio_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::TokioPath(input.parse()?))
        } else if l.peek(kw::tracing_path) {
            input.parse::<kw::tracing_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::TracingPath(input.parse()?))
        } else if l.peek(kw::wasm_tokio_path) {
            input.parse::<kw::wasm_tokio_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::WasmTokioPath(input.parse()?))
        } else if l.peek(kw::wrpc_transport_path) {
            input.parse::<kw::wrpc_transport_path>()?;
            input.parse::<Token![:]>()?;
            Ok(Opt::WrpcTransportPath(input.parse()?))
        } else if l.peek(kw::features) {
            input.parse::<kw::features>()?;
            input.parse::<Token![:]>()?;
            let contents;
            syn::bracketed!(contents in input);
            let list = Punctuated::<_, Token![,]>::parse_terminated(&contents)?;
            Ok(Opt::Features(list.into_iter().collect()))
        } else {
            Err(l.error())
        }
    }
}

fn with_field_parse(input: ParseStream<'_>) -> Result<(String, WithOption)> {
    let interface = input.parse::<syn::LitStr>()?.value();
    input.parse::<Token![:]>()?;
    let start = input.span();
    let path = input.parse::<syn::Path>()?;

    // It's not possible for the segments of a path to be empty
    let span = start
        .join(path.segments.last().unwrap().ident.span())
        .unwrap_or(start);

    if path.is_ident("generate") {
        return Ok((interface, WithOption::Generate));
    }

    let mut buf = String::new();
    let append = |buf: &mut String, segment: syn::PathSegment| -> Result<()> {
        if !segment.arguments.is_none() {
            return Err(Error::new(
                span,
                "Module path must not contain angles or parens",
            ));
        }

        buf.push_str(&segment.ident.to_string());

        Ok(())
    };

    if path.leading_colon.is_some() {
        buf.push_str("::");
    }

    let mut segments = path.segments.into_iter();

    if let Some(segment) = segments.next() {
        append(&mut buf, segment)?;
    }

    for segment in segments {
        buf.push_str("::");
        append(&mut buf, segment)?;
    }

    Ok((interface, WithOption::Path(buf)))
}

/// Format a valid Rust string
fn fmt(input: &str) -> Result<String> {
    let syntax_tree = syn::parse_file(input)?;
    Ok(prettyplease::unparse(&syntax_tree))
}
