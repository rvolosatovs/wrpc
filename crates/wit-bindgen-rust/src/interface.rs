use crate::{
    int_repr, to_rust_ident, to_upper_camel_case, FnSig, Identifier, InterfaceName, RustFlagsRepr,
    RustWrpc,
};
use heck::{ToShoutySnakeCase, ToUpperCamelCase};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::mem;
use wit_bindgen_core::wit_parser::{
    Case, Docs, Enum, Field, Flags, Function, FunctionKind, Handle, Int, InterfaceId, Record,
    Resolve, Result_, Stream, Tuple, Type, TypeDefKind, TypeId, TypeOwner, Variant, World,
    WorldKey,
};
use wit_bindgen_core::{uwrite, uwriteln, Source, TypeInfo};
use wrpc_introspect::{async_paths_ty, async_paths_tyid, is_ty, rpc_func_name};

pub struct InterfaceGenerator<'a> {
    pub src: Source,
    pub(super) identifier: Identifier<'a>,
    pub in_import: bool,
    pub(super) gen: &'a mut RustWrpc,
    pub resolve: &'a Resolve,
}

impl InterfaceGenerator<'_> {
    pub(super) fn generate_exports<'a>(
        &mut self,
        identifier: Identifier<'a>,
        funcs: impl Iterator<Item = &'a Function> + Clone,
    ) -> bool {
        let mut traits = BTreeMap::new();
        let mut funcs_to_export = Vec::new();

        traits.insert(None, ("Handler".to_string(), Vec::new()));

        if let Identifier::Interface(id, ..) = identifier {
            for (name, id) in &self.resolve.interfaces[id].types {
                match self.resolve.types[*id].kind {
                    TypeDefKind::Resource => {}
                    _ => continue,
                }
                let camel = name.to_upper_camel_case();
                traits.insert(Some(*id), (format!("Handler{camel}"), Vec::new()));
            }
        }

        for func in funcs {
            if self.gen.skip.contains(&func.name) {
                continue;
            }

            let resource = match func.kind {
                FunctionKind::Freestanding => None,
                FunctionKind::Method(id)
                | FunctionKind::Constructor(id)
                | FunctionKind::Static(id) => Some(id),
            };
            funcs_to_export.push(func);
            let (_, methods) = traits.get_mut(&resource).unwrap();

            let prev = mem::take(&mut self.src);
            let mut sig = FnSig {
                use_item_name: true,
                private: true,
                ..Default::default()
            };
            sig.self_arg = Some("&self".into());
            sig.self_is_first_param = false;

            self.print_docs_and_params(func, &sig);
            match func.results.len() {
                0 => {
                    uwrite!(
                        self.src,
                        " -> impl ::core::future::Future<Output = {}::Result<()>> + ::core::marker::Send",
                        self.gen.anyhow_path()
                    );
                }
                1 => {
                    uwrite!(
                        self.src,
                        " -> impl ::core::future::Future<Output = {}::Result<",
                        self.gen.anyhow_path()
                    );
                    let ty = func.results.iter_types().next().unwrap();
                    self.print_ty(ty, true, false);
                    self.push_str(">> + ::core::marker::Send");
                }
                _ => {
                    uwrite!(
                        self.src,
                        " -> impl ::core::future::Future<Output = {}::Result<(",
                        self.gen.anyhow_path()
                    );
                    for ty in func.results.iter_types() {
                        self.print_ty(ty, true, false);
                        self.push_str(", ");
                    }
                    self.push_str(")>> + ::core::marker::Send");
                }
            }
            self.src.push_str(";\n");
            let trait_method = mem::replace(&mut self.src, prev);
            methods.push(trait_method);
        }

        let (name, methods) = traits.remove(&None).unwrap();
        if !methods.is_empty() || !traits.is_empty() {
            let resource_traits: Vec<_> = traits
                .values()
                .map(|(name, _)| format!("{name}<Ctx>"))
                .collect();
            let resource_traits = resource_traits.join(" + ");

            uwriteln!(self.src, "pub trait {name}<Ctx>: {resource_traits} {{");
            for method in methods {
                self.src.push_str(&method);
            }
            uwriteln!(self.src, "}}");
        }

        for (trait_name, methods) in traits.values() {
            uwriteln!(self.src, "pub trait {trait_name}<Ctx>: 'static {{");
            for method in methods {
                self.src.push_str(method);
            }
            uwriteln!(self.src, "}}");
        }

        if funcs_to_export.is_empty() {
            return false;
        }

        let trait_names: Vec<_> = traits
            .values()
            .map(|(name, _)| format!("{name}<T::Context> + "))
            .collect();

        let n = funcs_to_export.len();
        uwrite!(
            self.src,
            r#"
#[allow(clippy::manual_async_fn)]
pub fn serve_interface<'a, T: {wrpc_transport}::Serve>(
    wrpc: &'a T,
    handler: impl Handler<T::Context> + {resource_traits} ::core::marker::Send + ::core::marker::Sync + ::core::clone::Clone + 'static,
) -> impl ::core::future::Future<
        Output = {anyhow}::Result<
            [
                (
                    &'static str,
                    &'static str,
                    ::core::pin::Pin<
                        ::std::boxed::Box<
                            dyn {futures}::Stream<
                                Item = {anyhow}::Result<
                                    ::core::pin::Pin<
                                        ::std::boxed::Box<
                                            dyn ::core::future::Future<
                                                Output = {anyhow}::Result<()>
                                            > + ::core::marker::Send + 'static
                                        >
                                    >
                                >
                            > + ::core::marker::Send + 'static
                        >
                    >
                );
                {n}
            ] 
        >
    > + ::core::marker::Send + {wrpc_transport}::Captures<'a> {{
    async move {{
        let ("#,
            resource_traits = trait_names.join(""),
            anyhow = self.gen.anyhow_path(),
            futures = self.gen.futures_path(),
            wrpc_transport = self.gen.wrpc_transport_path()
        );
        for Function { name, kind, .. } in &funcs_to_export {
            uwriteln!(
                self.src,
                "{}_{},",
                match kind {
                    FunctionKind::Freestanding => "f",
                    FunctionKind::Method(_) => "m",
                    FunctionKind::Constructor(_) => "c",
                    FunctionKind::Static(_) => "s",
                },
                to_rust_ident(name)
            );
        }
        uwrite!(
            self.src,
            ") = {tokio}::try_join!(",
            tokio = self.gen.tokio_path()
        );
        let instance = match identifier {
            Identifier::Interface(id, name) => {
                let interface = &self.resolve.interfaces[id];
                let name = match name {
                    WorldKey::Name(s) => s.to_string(),
                    WorldKey::Interface(..) => interface
                        .name
                        .as_ref()
                        .expect("interface name missing")
                        .to_string(),
                };
                if let Some(package) = interface.package {
                    self.resolve.id_of_name(package, &name)
                } else {
                    name
                }
            }
            Identifier::World(world) => {
                let World {
                    ref name, package, ..
                } = self.resolve.worlds[world];
                if let Some(package) = package {
                    self.resolve.id_of_name(package, name)
                } else {
                    name.to_string()
                }
            }
        };
        for func in &funcs_to_export {
            let paths = func.params.iter().enumerate().fold(
                BTreeSet::default(),
                |mut paths, (i, (_, ty))| {
                    let (nested, fut) = async_paths_ty(self.resolve, ty);
                    for path in nested {
                        let mut s = String::with_capacity(3 + path.len() * 6);
                        s.push_str(&format!("[Some({i})"));
                        for i in path {
                            if let Some(i) = i {
                                s.push_str(&format!(", Some({i})"));
                            } else {
                                s.push_str(", None");
                            }
                        }
                        s.push(']');
                        paths.insert(s);
                    }
                    if fut {
                        paths.insert(format!("[Some({i})]"));
                    }
                    paths
                },
            );
            uwrite!(
                self.src,
                r#"
            async {{ 
                {anyhow}::Context::context({wrpc_transport}::ServeExt::serve_values(
                    wrpc,
                    "{instance}",
                    "{}",
                    ::std::sync::Arc::from("#,
                rpc_func_name(func),
                anyhow = self.gen.anyhow_path(),
                wrpc_transport = self.gen.wrpc_transport_path(),
            );
            if paths.is_empty() {
                self.src.push_str(
                    r"{
                       let paths: [::std::boxed::Box<[::core::option::Option<usize>]>; 0] = [];
                       paths
                   }",
                );
            } else {
                self.src.push_str("[");
                for path in paths {
                    self.src.push_str("::std::boxed::Box::from(");
                    self.src.push_str(&path);
                    self.src.push_str("), ");
                }
                self.src.push_str("]");
            }
            uwriteln!(
                self.src,
                r#")
                )
                .await,
                "failed to serve `{instance}.{}`")
            }},"#,
                func.name,
            );
        }
        self.push_str(")?;\n");
        uwrite!(
            self.src,
            r"
        {anyhow}::Ok([",
            anyhow = self.gen.anyhow_path(),
        );
        for func in &funcs_to_export {
            let name = to_rust_ident(&func.name);
            uwrite!(
                self.src,
                r#"
            {{
                let handler = handler.clone();
                (
                    "{instance}",
                    "{wit_name}",
                    ::std::boxed::Box::pin(
                        {futures}::TryStreamExt::map_ok({}_{name}, move |(cx, ("#,
                match func.kind {
                    FunctionKind::Freestanding => "f",
                    FunctionKind::Method(_) => "m",
                    FunctionKind::Constructor(_) => "c",
                    FunctionKind::Static(_) => "s",
                },
                futures = self.gen.futures_path(),
                wit_name = func.name,
            );
            for i in 0..func.params.len() {
                uwrite!(self.src, "p{i}, ");
            }
            uwrite!(
                self.src,
                r"
                        ), rx, tx)| {{
                            let handler = handler.clone();
                            ::std::boxed::Box::pin(async move {{"
            );
            let (trait_name, name) = match func.kind {
                FunctionKind::Freestanding => ("Handler", name),
                FunctionKind::Method(id)
                | FunctionKind::Constructor(id)
                | FunctionKind::Static(id) => (
                    traits
                        .get(&Some(id))
                        .map(|(name, _)| name.as_str())
                        .unwrap(),
                    if let FunctionKind::Constructor(_) = func.kind {
                        "new".to_string()
                    } else {
                        to_rust_ident(func.item_name()).to_string()
                    },
                ),
            };
            uwrite!(
                self.src,
                r#"
                                let rx = rx.map({tokio}::spawn);
                                {tracing}::trace!(instance = "{instance}", func = "{wit_name}", "calling handler");
                                match {trait_name}::{name}(&handler, cx"#,
                tokio = self.gen.tokio_path(),
                tracing = self.gen.tracing_path(),
                wit_name = func.name,
            );
            for i in 0..func.params.len() {
                uwrite!(self.src, ", p{i}");
            }
            uwrite!(
                self.src,
                r"
                                ).await {{
                                    Ok(results) => {{
                                        match tx(",
            );
            if func.results.len() == 1 {
                // wrap single-element results into a tuple for correct indexing
                self.push_str("(results,)");
            } else {
                self.push_str("results");
            }
            uwrite!(
                self.src,
                r#"
                                        ).await {{
                                            Ok(()) => {{
                                                if let Some(rx) = rx {{
                                                    {tracing}::trace!(instance = "{instance}", func = "{wit_name}", "receiving async invocation parameters");
                                                    let rx = {anyhow}::Context::context(
                                                        rx.await,
                                                        "`{instance}.{wit_name}` async parameter receipt task failed",
                                                    )?;
                                                    {anyhow}::Context::context(
                                                        rx,
                                                        "failed to receive `{instance}.{wit_name}` async parameters",
                                                    )
                                                }} else {{
                                                    {anyhow}::Ok(())
                                                }}
                                            }},
                                            Err(err) => {{
                                                if let Some(rx) = rx {{
                                                    rx.abort();
                                                }}
                                                {anyhow}::bail!("failed to transmit `{instance}.{wit_name}` invocation results")
                                            }},
                                        }}
                                    }},
                                    Err(err) => {{
                                        if let Some(rx) = rx {{
                                            rx.abort();
                                        }}
                                        {anyhow}::bail!("failed to serve `{instance}.{wit_name}` invocation")
                                    }},
                                }}
                            }})
                            as ::core::pin::Pin<
                                    ::std::boxed::Box<
                                        dyn ::core::future::Future<
                                            Output = {anyhow}::Result<()>
                                    > + ::core::marker::Send + 'static
                                >
                            >
                        }})
                    )
                    as ::core::pin::Pin<
                        ::std::boxed::Box<
                            dyn {futures}::Stream<
                                Item = {anyhow}::Result<
                                    ::core::pin::Pin<
                                        ::std::boxed::Box<
                                            dyn ::core::future::Future<
                                                Output = {anyhow}::Result<()>
                                            > + ::core::marker::Send + 'static
                                        >
                                    >
                                >
                            > + ::core::marker::Send + 'static
                        >
                    >
                )
            }},"#,
                anyhow = self.gen.anyhow_path(),
                futures = self.gen.futures_path(),
                tracing = self.gen.tracing_path(),
                wit_name = func.name,
            );
        }

        uwriteln!(
            self.src,
            r#"
        ])
    }}
}}"#,
        );
        true
    }

    pub fn generate_imports<'a>(
        &mut self,
        instance: &str,
        funcs: impl Iterator<Item = &'a Function>,
    ) {
        for func in funcs {
            self.generate_guest_import(instance, func);
        }
    }

    pub fn finish(&mut self) -> String {
        mem::take(&mut self.src).into()
    }

    fn path_to_root(&self) -> String {
        let mut path_to_root = String::new();

        if let Identifier::Interface(_, key) = self.identifier {
            // Escape the submodule for this interface
            path_to_root.push_str("super::");

            // Escape the `exports` top-level submodule
            if !self.in_import {
                path_to_root.push_str("super::");
            }

            // Escape the namespace/package submodules for interface-based ids
            match key {
                WorldKey::Name(_) => {}
                WorldKey::Interface(_) => {
                    path_to_root.push_str("super::super::");
                }
            }
        }
        path_to_root
    }

    pub fn start_append_submodule(&mut self, name: &WorldKey) -> (String, Vec<String>) {
        let snake = match name {
            WorldKey::Name(name) => to_rust_ident(name),
            WorldKey::Interface(id) => {
                to_rust_ident(self.resolve.interfaces[*id].name.as_ref().unwrap())
            }
        };
        let module_path = crate::compute_module_path(name, self.resolve, !self.in_import);
        (snake, module_path)
    }

    pub fn finish_append_submodule(mut self, snake: &str, module_path: Vec<String>) {
        let module = self.finish();
        let module = format!(
            "\
                #[allow(dead_code, clippy::all)]
                pub mod {snake} {{
                    {module}
                }}
",
        );
        let map = if self.in_import {
            &mut self.gen.import_modules
        } else {
            &mut self.gen.export_modules
        };
        map.push((module, module_path));
    }

    fn generate_guest_import(&mut self, instance: &str, func: &Function) {
        if self.gen.skip.contains(&func.name) {
            return;
        }

        let mut sig = FnSig::default();
        match func.kind {
            FunctionKind::Method(id) => {
                let name = self.resolve.types[id].name.as_ref().unwrap();
                let name = to_upper_camel_case(name);
                uwriteln!(self.src, "impl {name} {{");

                sig.use_item_name = true;
                sig.self_arg = Some("&self".into());
                sig.self_is_first_param = true;
            }
            FunctionKind::Static(id) | FunctionKind::Constructor(id) => {
                let name = self.resolve.types[id].name.as_ref().unwrap();
                let name = to_upper_camel_case(name);
                uwriteln!(self.src, "impl {name} {{");

                sig.use_item_name = true;
            }
            FunctionKind::Freestanding => {}
        }
        self.src.push_str("#[allow(clippy::all)]\n");

        let async_params = func.params.iter().any(|(_, ty)| {
            let (paths, fut) = async_paths_ty(self.resolve, ty);
            fut || !paths.is_empty()
        });
        let paths = func.results.iter_types().enumerate().fold(
            BTreeSet::default(),
            |mut paths, (i, ty)| {
                let (nested, fut) = async_paths_ty(self.resolve, ty);
                for path in nested {
                    let mut s = String::with_capacity(7 + path.len() * 6);
                    s.push_str(&format!("[Some({i})"));
                    for i in path {
                        if let Some(i) = i {
                            s.push_str(&format!(", Some({i})"));
                        } else {
                            s.push_str(", None");
                        }
                    }
                    s.push(']');
                    paths.insert(s);
                }
                if fut {
                    paths.insert(format!("[Some({i})]"));
                }
                paths
            },
        );

        let anyhow = self.gen.anyhow_path().to_string();

        let params = self.print_docs_and_params(func, &sig);
        match func.results.iter_types().collect::<Vec<_>>().as_slice() {
            [] => {
                uwrite!(
                    self.src,
                    " -> impl ::core::future::Future<Output = {anyhow}::Result<()>> + Send + 'a"
                );
            }
            [ty] => {
                uwrite!(
                    self.src,
                    " -> impl ::core::future::Future<Output = {anyhow}::Result<"
                );
                if async_params || !paths.is_empty() {
                    self.push_str("(");
                }
                self.print_ty(ty, true, false);
                if async_params || !paths.is_empty() {
                    uwrite!(
                        self.src,
                        ", ::core::option::Option<impl ::core::future::Future<Output = {anyhow}::Result<()>> + ::core::marker::Send + 'static + {wrpc_transport}::Captures<'a>>)",
                        wrpc_transport = self.gen.wrpc_transport_path(),
                    );
                }
                uwrite!(self.src, ">> + Send + 'a");
            }
            types => {
                uwrite!(
                    self.src,
                    " -> impl ::core::future::Future<Output = {anyhow}::Result<(",
                );
                for ty in types {
                    self.print_ty(ty, true, false);
                    self.push_str(", ");
                }
                if async_params || !paths.is_empty() {
                    uwrite!(
                        self.src,
                        "::core::option::Option<impl ::core::future::Future<Output = {anyhow}::Result<()>> + ::core::marker::Send + 'static + {wrpc_transport}::Captures<'a>>",
                        wrpc_transport = self.gen.wrpc_transport_path(),
                    );
                }
                uwrite!(self.src, ")>> + Send + 'a");
            }
        };
        self.push_str(
            r"
        {
        async move {",
        );

        if func.results.len() == 0 || (!async_params && paths.is_empty()) {
            uwrite!(
                self.src,
                r#"
                let wrpc__ = {anyhow}::Context::context(
                    {wrpc_transport}::SendFuture::send({wrpc_transport}::InvokeExt::invoke_values_blocking(wrpc__, cx__, "{instance}", "{}", ({params}), "#,
                rpc_func_name(func),
                wrpc_transport = self.gen.wrpc_transport_path(),
                params = {
                    let s = params.join(", ");
                    if params.len() == 1 {
                        format!("{s},")
                    } else {
                        s
                    }
                },
            );
            self.src.push_str("[");
            if paths.is_empty() {
                self.src.push_str("[None; 0]; 0");
            } else {
                for path in paths {
                    self.src.push_str(&path);
                    self.src.push_str(".as_slice(), ");
                }
            }
            self.src.push_str("])).await,\n");
            uwriteln!(
                self.src,
                r#"
                    "failed to invoke `{instance}.{}`")?;"#,
                func.name,
            );
            if func.results.len() == 1 {
                self.push_str("let (wrpc__,) = wrpc__;\n");
            }
            self.push_str("Ok(wrpc__)\n");
        } else {
            uwrite!(
                self.src,
                r#"
                let (wrpc__, io__) = {anyhow}::Context::context(
                    {wrpc_transport}::SendFuture::send({wrpc_transport}::InvokeExt::invoke_values(wrpc__, cx__, "{instance}", "{}", ({params}), "#,
                rpc_func_name(func),
                wrpc_transport = self.gen.wrpc_transport_path(),
                params = {
                    let s = params.join(", ");
                    if params.len() == 1 {
                        format!("{s},")
                    } else {
                        s
                    }
                },
            );
            self.src.push_str("[");
            if paths.is_empty() {
                self.src.push_str("[None; 0]; 0");
            } else {
                for path in paths {
                    self.src.push_str(&path);
                    self.src.push_str(".as_slice(), ");
                }
            }
            self.src.push_str("])).await,\n");
            uwriteln!(
                self.src,
                r#"
                    "failed to invoke `{instance}.{}`")?;"#,
                func.name,
            );
            if func.results.len() == 1 {
                self.push_str("let (wrpc__,) = wrpc__;\n");
                self.push_str("Ok((wrpc__, io__))\n");
            } else {
                self.push_str("let (");
                for i in 0..func.results.len() {
                    uwrite!(self.src, "r{i}__, ");
                }
                self.push_str(") = wrpc__;\n");
                self.push_str("Ok((");
                for i in 0..func.results.len() {
                    uwrite!(self.src, "r{i}__, ");
                }
                self.push_str("io__))\n");
            }
        }
        uwriteln!(
            self.src,
            r"
            }}
        }}"
        );

        match func.kind {
            FunctionKind::Freestanding => {}
            FunctionKind::Method(_) | FunctionKind::Static(_) | FunctionKind::Constructor(_) => {
                self.src.push_str("}\n");
            }
        }
    }

    fn rustdoc(&mut self, docs: &Docs) {
        let docs = match &docs.contents {
            Some(docs) => docs,
            None => return,
        };
        for line in docs.trim().lines() {
            self.push_str("///");
            if !line.is_empty() {
                self.push_str(" ");
                self.push_str(line);
            }
            self.push_str("\n");
        }
    }

    fn rustdoc_params(&mut self, docs: &[(String, Type)], header: &str) {
        let _ = (docs, header);
        // let docs = docs
        //     .iter()
        //     .filter(|param| param.docs.trim().len() > 0)
        //     .collect::<Vec<_>>();
        // if docs.len() == 0 {
        //     return;
        // }

        // self.push_str("///\n");
        // self.push_str("/// ## ");
        // self.push_str(header);
        // self.push_str("\n");
        // self.push_str("///\n");

        // for param in docs {
        //     for (i, line) in param.docs.lines().enumerate() {
        //         self.push_str("/// ");
        //         // Currently wasi only has at most one return value, so there's no
        //         // need to indent it or name it.
        //         if header != "Return" {
        //             if i == 0 {
        //                 self.push_str("* `");
        //                 self.push_str(to_rust_ident(param.name.as_str()));
        //                 self.push_str("` - ");
        //             } else {
        //                 self.push_str("  ");
        //             }
        //         }
        //         self.push_str(line);
        //         self.push_str("\n");
        //     }
        // }
    }

    fn print_docs_and_params(&mut self, func: &Function, sig: &FnSig) -> Vec<String> {
        self.rustdoc(&func.docs);
        self.rustdoc_params(&func.params, "Parameters");
        // TODO: re-add this when docs are back
        // self.rustdoc_params(&func.results, "Return");

        if !sig.private {
            self.push_str("pub ");
        }
        self.push_str("fn ");
        if sig.use_item_name {
            if let FunctionKind::Constructor(_) = &func.kind {
                self.push_str("new");
            } else {
                self.push_str(&to_rust_ident(func.item_name()));
            }
        } else {
            self.push_str(&to_rust_ident(&func.name));
        };
        if self.in_import {
            uwrite!(
                self.src,
                "<'a, C: {wrpc_transport}::Invoke>(wrpc__: &'a C, cx__: C::Context,",
                wrpc_transport = self.gen.wrpc_transport_path(),
            );
        } else {
            self.push_str("(");
            if let Some(arg) = &sig.self_arg {
                self.push_str(arg);
                self.push_str(",");
            }
            self.push_str("cx: Ctx,");
        }
        let mut params = Vec::new();
        for (i, (name, param)) in func.params.iter().enumerate() {
            if i == 0 && sig.self_is_first_param && !self.in_import {
                continue;
            }

            let name = to_rust_ident(name);
            self.push_str(&name);
            self.push_str(": ");

            if self.in_import {
                self.print_import_param_ty(param);
            } else {
                self.print_ty(param, true, false);
            }
            params.push(name);
            self.push_str(",");
        }
        self.push_str(")");
        params
    }

    fn print_ty(&mut self, ty: &Type, owned: bool, submodule: bool) {
        match ty {
            Type::Id(t) => self.print_tyid(*t, owned, submodule),
            Type::Bool => self.push_str("bool"),
            Type::U8 => self.push_str("u8"),
            Type::U16 => self.push_str("u16"),
            Type::U32 => self.push_str("u32"),
            Type::U64 => self.push_str("u64"),
            Type::S8 => self.push_str("i8"),
            Type::S16 => self.push_str("i16"),
            Type::S32 => self.push_str("i32"),
            Type::S64 => self.push_str("i64"),
            Type::F32 => self.push_str("f32"),
            Type::F64 => self.push_str("f64"),
            Type::Char => self.push_str("char"),
            Type::String => {
                if owned {
                    self.push_str("String");
                } else {
                    self.push_str("&'a str");
                }
            }
        }
    }

    fn print_optional_ty(&mut self, ty: Option<&Type>, owned: bool, submodule: bool) {
        match ty {
            Some(ty) => {
                self.print_ty(ty, owned, submodule);
            }
            None => self.push_str("()"),
        }
    }

    fn type_path_with_name(&self, id: TypeId, name: String, submodule: bool) -> String {
        if let TypeOwner::Interface(id) = self.resolve.types[id].owner {
            if let Some(path) = self.path_to_interface(id) {
                if submodule {
                    return format!("super::{path}::{name}");
                } else {
                    return format!("{path}::{name}");
                }
            }
        }
        if submodule {
            format!("super::{name}")
        } else {
            name
        }
    }

    fn print_import_param_ty(&mut self, ty: &Type) {
        match ty {
            Type::Id(id) => {
                let ty = &self.resolve.types[*id];
                match &ty.kind {
                    TypeDefKind::Tuple(ty) => self.print_tuple(ty, false, false),
                    TypeDefKind::Enum(..) => {
                        let name = ty.name.as_ref().expect("enum missing a name");
                        let name = self.type_path_with_name(*id, to_upper_camel_case(name), false);
                        self.push_str(&name);
                    }
                    TypeDefKind::Option(ty) => self.print_option(ty, false, false),
                    TypeDefKind::List(ty) => self.print_list(ty, false, false),
                    TypeDefKind::Future(ty) => self.print_future(ty, false),
                    TypeDefKind::Stream(ty) => self.print_stream(ty, false),
                    TypeDefKind::Type(ty) => self.print_import_param_ty(ty),
                    _ => {
                        let (paths, fut) = async_paths_tyid(self.resolve, *id);
                        if !fut && paths.is_empty() {
                            self.push_str("&'a ");
                        }
                        self.print_tyid(*id, true, false);
                    }
                }
            }
            Type::Bool => self.push_str("bool"),
            Type::U8 => self.push_str("u8"),
            Type::U16 => self.push_str("u16"),
            Type::U32 => self.push_str("u32"),
            Type::U64 => self.push_str("u64"),
            Type::S8 => self.push_str("i8"),
            Type::S16 => self.push_str("i16"),
            Type::S32 => self.push_str("i32"),
            Type::S64 => self.push_str("i64"),
            Type::F32 => self.push_str("f32"),
            Type::F64 => self.push_str("f64"),
            Type::Char => self.push_str("char"),
            Type::String => self.push_str("&'a str"),
        }
    }

    fn print_tuple(&mut self, Tuple { types }: &Tuple, owned: bool, submodule: bool) {
        self.push_str("(");
        for ty in types {
            self.print_ty(ty, owned, submodule);
            self.push_str(",");
        }
        self.push_str(")");
    }

    fn print_option(&mut self, ty: &Type, owned: bool, submodule: bool) {
        self.push_str("::core::option::Option<");
        self.print_ty(ty, owned, submodule);
        self.push_str(">");
    }

    fn print_result(&mut self, ty: &Result_, owned: bool, submodule: bool) {
        self.push_str("::core::result::Result<");
        self.print_optional_ty(ty.ok.as_ref(), owned, submodule);
        self.push_str(",");
        self.print_optional_ty(ty.err.as_ref(), owned, submodule);
        self.push_str(">");
    }

    fn print_list(&mut self, ty: &Type, owned: bool, submodule: bool) {
        if owned {
            if is_ty(self.resolve, Type::U8, ty) {
                uwrite!(self.src, "{bytes}::Bytes", bytes = self.gen.bytes_path());
            } else {
                self.push_str("Vec<");
                self.print_ty(ty, true, submodule);
                self.push_str(">");
            }
        } else if is_ty(self.resolve, Type::U8, ty) {
            uwrite!(
                self.src,
                "&'a {bytes}::Bytes",
                bytes = self.gen.bytes_path()
            );
        } else {
            self.push_str("&'a [");
            self.print_ty(ty, false, submodule);
            self.push_str("]");
        }
    }

    fn print_future(&mut self, ty: &Option<Type>, submodule: bool) {
        self.push_str("::core::pin::Pin<::std::boxed::Box<dyn ::core::future::Future<Output = ");
        if let Some(ty) = ty {
            self.print_ty(ty, true, submodule);
        } else {
            self.push_str("()");
        }
        self.push_str("> + ::core::marker::Send>>");
    }

    fn print_stream(&mut self, Stream { element, .. }: &Stream, submodule: bool) {
        uwrite!(
            self.src,
            "::core::pin::Pin<::std::boxed::Box<dyn {futures}::Stream<Item = ",
            futures = self.gen.futures_path()
        );
        if let Some(ty) = element {
            self.print_list(ty, true, submodule);
        } else {
            self.push_str("Vec<()>");
        }
        self.push_str("> + ::core::marker::Send>>");
    }

    fn print_own(&mut self, id: TypeId, submodule: bool) {
        self.src.push_str(self.gen.wrpc_transport_path());
        self.push_str("::ResourceOwn<");
        self.print_tyid(id, true, submodule);
        self.push_str(">");
    }

    fn print_borrow(&mut self, id: TypeId, submodule: bool) {
        self.src.push_str(self.gen.wrpc_transport_path());
        self.push_str("::ResourceBorrow<");
        self.print_tyid(id, true, submodule);
        self.push_str(">");
    }

    fn print_tyid(&mut self, id: TypeId, owned: bool, submodule: bool) {
        let ty = &self.resolve.types[id];
        if let Some(name) = &ty.name {
            let name = self.type_path_with_name(id, to_upper_camel_case(name), submodule);
            self.push_str(&name);
            return;
        }
        match &ty.kind {
            TypeDefKind::List(ty) => self.print_list(ty, owned, submodule),
            TypeDefKind::Option(ty) => self.print_option(ty, owned, submodule),
            TypeDefKind::Result(ty) => self.print_result(ty, owned, submodule),
            TypeDefKind::Variant(_) => panic!("unsupported anonymous variant"),
            TypeDefKind::Tuple(ty) => self.print_tuple(ty, owned, submodule),
            TypeDefKind::Resource => panic!("unsupported anonymous type reference: resource"),
            TypeDefKind::Record(_) => panic!("unsupported anonymous type reference: record"),
            TypeDefKind::Flags(_) => panic!("unsupported anonymous type reference: flags"),
            TypeDefKind::Enum(_) => panic!("unsupported anonymous type reference: enum"),
            TypeDefKind::Future(ty) => self.print_future(ty, submodule),
            TypeDefKind::Stream(ty) => self.print_stream(ty, submodule),
            TypeDefKind::Handle(Handle::Own(id)) => self.print_own(*id, submodule),
            TypeDefKind::Handle(Handle::Borrow(id)) => self.print_borrow(*id, submodule),
            TypeDefKind::Type(t) => self.print_ty(t, owned, submodule),
            TypeDefKind::Unknown => unreachable!(),
        }
    }

    fn int_repr(&mut self, repr: Int) {
        self.push_str(int_repr(repr));
    }

    fn name_of(&self, ty: TypeId) -> Option<String> {
        (self.gen.opts.generate_unused_types
            // If this type isn't actually used, no need to generate it.
            || matches!(
                self.info(ty),
                TypeInfo { owned: true, .. } | TypeInfo { borrowed: true, .. }
            ))
        .then(|| to_upper_camel_case(self.resolve.types[ty].name.as_ref().unwrap()))
    }

    fn print_rust_enum_debug<'b>(
        &mut self,
        name: &str,
        cases: impl IntoIterator<Item = (String, Option<&'b Type>)>,
    ) where
        Self: Sized,
    {
        self.push_str("impl");
        self.push_str(" ::core::fmt::Debug for ");
        self.push_str(name);
        self.push_str(" {\n");
        self.push_str(
            "fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {\n",
        );
        self.push_str("match self {\n");
        for (case_name, payload) in cases {
            self.push_str(name);
            self.push_str("::");
            self.push_str(&case_name);
            let is_async = payload.is_some_and(|ty| async_paths_ty(self.resolve, ty).1);
            if payload.is_some() {
                if is_async {
                    self.push_str("(_)");
                } else {
                    self.push_str("(e)");
                }
            }
            self.push_str(" => {\n");
            self.push_str(&format!("f.debug_tuple(\"{name}::{case_name}\")"));
            if payload.is_some() {
                if is_async {
                    self.push_str(r#".field(&"<async>")"#);
                } else {
                    self.push_str(".field(e)");
                }
            }
            self.push_str(".finish()\n");
            self.push_str("}\n");
        }
        self.push_str("}\n");
        self.push_str("}\n");
        self.push_str("}\n");
    }

    fn path_to_interface(&self, interface: InterfaceId) -> Option<String> {
        let InterfaceName { path, remapped } = &self.gen.interface_names[&interface];
        if *remapped {
            let mut path_to_root = self.path_to_root();
            path_to_root.push_str(path);
            Some(path_to_root)
        } else {
            let mut full_path = String::new();
            if let Identifier::Interface(cur, name) = self.identifier {
                if cur == interface {
                    return None;
                }
                if !self.in_import {
                    full_path.push_str("super::");
                }
                match name {
                    WorldKey::Name(_) => {
                        full_path.push_str("super::");
                    }
                    WorldKey::Interface(_) => {
                        full_path.push_str("super::super::super::");
                    }
                }
            }
            full_path.push_str(path);
            Some(full_path)
        }
    }

    fn push_str(&mut self, s: &str) {
        self.src.push_str(s);
    }

    fn info(&self, ty: TypeId) -> TypeInfo {
        self.gen.types.get(ty)
    }
}

impl<'a> wit_bindgen_core::InterfaceGenerator<'a> for InterfaceGenerator<'a> {
    fn resolve(&self) -> &'a Resolve {
        self.resolve
    }

    fn type_record(
        &mut self,
        id: TypeId,
        ty_name: &str,
        Record { fields, .. }: &Record,
        docs: &Docs,
    ) {
        let info = self.info(id);
        // We use a BTree set to make sure we don't have any duplicates and we have a stable order
        let additional_derives: BTreeSet<String> = self
            .gen
            .opts
            .additional_derive_attributes
            .iter()
            .cloned()
            .collect();
        if let Some(name) = self.name_of(id) {
            let (paths, _) = async_paths_tyid(self.resolve, id);

            self.rustdoc(docs);
            let mut derives = additional_derives.clone();
            if info.is_copy() && paths.is_empty() {
                self.push_str("#[repr(C)]\n");
                if !derives.contains("Clone")
                    && !derives.contains("core :: clone :: Clone")
                    && !derives.contains(":: core :: clone :: Clone")
                {
                    derives.insert("::core::clone::Clone".to_string());
                }
                if !derives.contains("Copy")
                    && !derives.contains("core :: marker :: Copy")
                    && !derives.contains(":: core :: marker :: Copy")
                {
                    derives.insert("::core::marker::Copy".to_string());
                }
            } else if info.is_clone()
                && paths.is_empty()
                && !derives.contains("Clone")
                && !derives.contains("core :: clone :: Clone")
                && !derives.contains(":: core :: clone :: Clone")
            {
                derives.insert("::core::clone::Clone".to_string());
            }
            if !derives.is_empty() {
                self.push_str("#[derive(");
                self.push_str(&derives.into_iter().collect::<Vec<_>>().join(", "));
                self.push_str(")]\n");
            }
            uwriteln!(self.src, "pub struct {name} {{");
            for Field { name, ty, docs } in fields {
                self.rustdoc(docs);
                self.push_str("pub ");
                self.push_str(&to_rust_ident(name));
                self.push_str(": ");
                self.print_ty(ty, true, false);
                self.push_str(",\n");
            }
            self.push_str("}\n");

            let mod_name = to_rust_ident(ty_name);

            let bytes = self.gen.bytes_path().to_string();
            let tokio = self.gen.tokio_path().to_string();
            let tokio_util = self.gen.tokio_util_path().to_string();
            let wrpc_transport = self.gen.wrpc_transport_path().to_string();

            let (paths, _) = async_paths_tyid(self.resolve, id);
            if paths.is_empty() {
                uwriteln!(
                    self.src,
                    r"
impl<W> {wrpc_transport}::Encode<W> for &self::{name}
where
    W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
{{
    type Encoder = {mod_name}::Encoder<W>;
}}",
                );
            }
            uwriteln!(
                self.src,
                r"
impl<W> {wrpc_transport}::Encode<W> for self::{name}
where
    W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
{{
    type Encoder = {mod_name}::Encoder<W>;
}}

impl<R> {wrpc_transport}::Decode<R> for self::{name}
where
    R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
{{
    type Decoder = {mod_name}::Decoder<R>;
    type ListDecoder = {wrpc_transport}::ListDecoder<Self::Decoder, R>;
}}

mod {mod_name} {{
    pub struct Encoder<W> 
    where
        W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
    {{",
            );

            if !fields.is_empty() {
                for Field { name, ty, .. } in fields {
                    let name = to_rust_ident(name);
                    uwrite!(self.src, "{name}: <");
                    self.print_ty(ty, true, true);
                    uwriteln!(self.src, " as {wrpc_transport}::Encode<W>>::Encoder,");
                }
            } else {
                uwriteln!(self.src, "_ty: ::core::marker::PhantomData<W>,");
            }
            uwriteln!(
                self.src,
                r"}}
    #[automatically_derived]
    impl<W> ::core::default::Default for Encoder<W>
    where
        W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
    {{
        fn default() -> Self {{
            Self{{"
            );
            if !fields.is_empty() {
                for Field { name, .. } in fields {
                    let name = to_rust_ident(name);
                    uwriteln!(self.src, "{name}: ::core::default::Default::default(),");
                }
            } else {
                uwriteln!(self.src, "_ty: ::core::marker::PhantomData,");
            }
            uwriteln!(
                self.src,
                r"}}
        }}
    }}

    #[automatically_derived]
    impl<W> {wrpc_transport}::Deferred<W> for Encoder<W>
    where
        W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
    {{
        fn take_deferred(&mut self) -> ::core::option::Option<{wrpc_transport}::DeferredFn<W>> {{"
            );
            if !fields.is_empty() {
                for Field { name, .. } in fields {
                    let name = to_rust_ident(name);
                    uwriteln!(self.src, "let f_{name} = self.{name}.take_deferred();");
                }
                self.push_str("if false");
                for Field { name, .. } in fields {
                    let name = to_rust_ident(name);
                    uwrite!(self.src, " || f_{name}.is_some()");
                }
                uwriteln!(
                    self.src,
                    r"{{
            return Some(::std::boxed::Box::new(|w, path| ::std::boxed::Box::pin(async move {{
                {tokio}::try_join!(",
                );
                for (i, Field { name, .. }) in fields.iter().enumerate() {
                    let name = to_rust_ident(name);
                    uwriteln!(
                        self.src,
                        r"async {{
                            let w = ::std::sync::Arc::clone(&w);
                            let Some(fut) = f_{name}
                            else {{
                                return Ok(())
                            }};
                            let mut path = path.clone();
                            path.push({i});
                            fut(w, path).await
                    }},"
                    );
                }
                uwriteln!(
                    self.src,
                    r"
                        )?;
                    Ok(())
                }})))
            }}",
                );
            }
            uwriteln!(
                self.src,
                r"
            None
        }}
    }}

    pub struct Decoder<R>
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{"
            );

            if !fields.is_empty() {
                for Field { name, ty, .. } in fields {
                    let name = to_rust_ident(name);
                    uwrite!(self.src, "c_{name}: <");
                    self.print_ty(ty, true, true);
                    uwriteln!(self.src, " as {wrpc_transport}::Decode<R>>::Decoder,");
                    uwrite!(self.src, "v_{name}: ::core::option::Option<");
                    self.print_ty(ty, true, true);
                    uwriteln!(self.src, ">,");
                }
            } else {
                uwriteln!(self.src, "_ty: ::core::marker::PhantomData<R>,");
            }
            uwriteln!(
                self.src,
                r"}}
    #[automatically_derived]
    impl<R> ::core::default::Default for Decoder<R>
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{
        fn default() -> Self {{
            Self{{"
            );
            if !fields.is_empty() {
                for Field { name, .. } in fields {
                    let name = to_rust_ident(name);
                    uwriteln!(self.src, "c_{name}: ::core::default::Default::default(),");
                    uwriteln!(self.src, "v_{name}: ::core::default::Default::default(),");
                }
            } else {
                uwriteln!(self.src, "_ty: ::core::marker::PhantomData,");
            }
            uwriteln!(
                self.src,
                r"}}
        }}
    }}

    #[automatically_derived]
    impl<R> {wrpc_transport}::Deferred<R> for Decoder<R>
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{
        fn take_deferred(&mut self) -> ::core::option::Option<{wrpc_transport}::DeferredFn<R>> {{"
            );
            if !fields.is_empty() {
                for Field { name, .. } in fields {
                    let name = to_rust_ident(name);
                    uwriteln!(self.src, "let f_{name} = self.c_{name}.take_deferred();");
                }
                self.push_str("if false");
                for Field { name, .. } in fields {
                    let name = to_rust_ident(name);
                    uwrite!(self.src, " || f_{name}.is_some()");
                }
                uwriteln!(
                    self.src,
                    r"{{
            return Some(::std::boxed::Box::new(|r, path| ::std::boxed::Box::pin(async move {{
                {tokio}::try_join!(",
                );
                for (i, Field { name, .. }) in fields.iter().enumerate() {
                    let name = to_rust_ident(name);
                    uwriteln!(
                        self.src,
                        r"async {{
                            let r = ::std::sync::Arc::clone(&r);
                            let Some(fut) = f_{name}
                            else {{
                                return Ok(())
                            }};
                            let mut path = path.clone();
                            path.push({i});
                            fut(r, path).await
                    }},"
                    );
                }
                uwriteln!(
                    self.src,
                    r"
                        )?;
                    Ok(())
                }})))
            }}",
                );
            }
            uwriteln!(
                self.src,
                r#"
            None
        }}
    }}

    #[automatically_derived]
    impl<R> {tokio_util}::codec::Decoder for Decoder<R> 
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{
        type Item = super::{name};
        type Error = ::std::io::Error;

        fn decode(&mut self, src: &mut {bytes}::BytesMut) -> ::core::result::Result<::core::option::Option<Self::Item>, Self::Error> {{"#
            );
            for Field { name, .. } in fields {
                let name = to_rust_ident(name);
                uwriteln!(
                    self.src,
                    r"
                if self.v_{name}.is_none() {{
                    let Some(v) = self.c_{name}.decode(src)? else {{
                        return Ok(None)
                    }};
                    self.v_{name} = Some(v);
                }}"
                );
            }
            self.push_str("Ok(Some(Self::Item{\n");
            for Field { name, .. } in fields {
                let name = to_rust_ident(name);
                uwriteln!(self.src, "{name}: self.v_{name}.take().unwrap(),");
            }
            self.push_str("}))\n");
            self.push_str("}\n");
            self.push_str("}\n");

            let mut names = vec![format!("super::{name}")];
            if paths.is_empty() {
                names.push(format!("&super::{name}"));
            }
            for name in names {
                uwriteln!(
                    self.src,
                    r#"
    #[automatically_derived]
    impl<W> {tokio_util}::codec::Encoder<{name}> for Encoder<W> 
    where
        W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
    {{
        type Error = ::std::io::Error;

        #[allow(unused_mut)]
        fn encode(&mut self, item: {name}, mut dst: &mut {bytes}::BytesMut) -> ::std::io::Result<()> {{"#
                );
                if !fields.is_empty() {
                    self.push_str("let ");
                    self.push_str(name.trim_start_matches('&'));
                    self.push_str("{\n");
                    for Field { name, .. } in fields {
                        let name = to_rust_ident(name);
                        uwriteln!(self.src, "{name}: f_{name},");
                    }
                    self.push_str("} = item;\n");

                    for Field { name, .. } in fields {
                        let name = to_rust_ident(name);
                        uwriteln!(self.src, "self.{name}.encode(f_{name}, &mut dst)?;");
                    }
                    uwriteln!(
                        self.src,
                        r"
        Ok(())
    }}
}}",
                    );
                }
            }
            uwriteln!(self.src, "}}");

            self.push_str("impl");
            self.push_str(" ::core::fmt::Debug for ");
            self.push_str(&name);
            self.push_str(" {\n");
            self.push_str(
                "fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {\n",
            );
            self.push_str(&format!("f.debug_struct(\"{name}\")"));
            for field in fields {
                let (_, fut) = async_paths_ty(self.resolve, &field.ty);
                if fut {
                    self.push_str(&format!(r#".field("{}", &"<async>")"#, field.name));
                } else {
                    self.push_str(&format!(
                        ".field(\"{}\", &self.{})",
                        field.name,
                        to_rust_ident(&field.name)
                    ));
                }
            }
            self.push_str(".finish()\n");
            self.push_str("}\n");
            self.push_str("}\n");

            if info.error {
                self.push_str("impl");
                self.push_str(" ::core::fmt::Display for ");
                self.push_str(&name);
                self.push_str(" {\n");
                self.push_str(
                    "fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {\n",
                );
                self.push_str("write!(f, \"{:?}\", self)\n");
                self.push_str("}\n");
                self.push_str("}\n");
                self.push_str("impl ::std::error::Error for ");
                self.push_str(&name);
                self.push_str(" {}\n");
            }
        }
    }

    fn type_resource(&mut self, _id: TypeId, name: &str, docs: &Docs) {
        self.rustdoc(docs);
        uwriteln!(
            self.src,
            r"
#[repr(transparent)]
pub struct {}(());",
            to_upper_camel_case(name)
        );
    }

    fn type_tuple(&mut self, id: TypeId, _name: &str, tuple: &Tuple, docs: &Docs) {
        if let Some(name) = self.name_of(id) {
            self.rustdoc(docs);
            uwrite!(self.src, "pub type {name} = ");
            self.print_tuple(tuple, true, false);
            self.push_str(";\n");
        }
    }

    fn type_flags(&mut self, id: TypeId, ty_name: &str, flags: &Flags, docs: &Docs) {
        if let Some(name) = self.name_of(id) {
            let bitflags = self.gen.bitflags_path().to_string();
            let bytes = self.gen.bytes_path().to_string();
            let tokio_util = self.gen.tokio_util_path().to_string();
            let wasm_tokio = self.gen.wasm_tokio_path().to_string();
            let wrpc_transport = self.gen.wrpc_transport_path().to_string();

            let mod_name = to_rust_ident(ty_name);

            uwriteln!(self.src, "{bitflags}::bitflags! {{");
            self.rustdoc(docs);
            let repr = RustFlagsRepr::new(flags);
            uwriteln!(
                self.src,
                "#[derive(::core::cmp::PartialEq, ::core::cmp::Eq, ::core::cmp::PartialOrd, ::core::cmp::Ord, ::core::hash::Hash, ::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]"
            );
            uwriteln!(self.src, "pub struct {name}: {repr} {{");

            for (i, flag) in flags.flags.iter().enumerate() {
                self.rustdoc(&flag.docs);
                uwriteln!(
                    self.src,
                    "const {} = 1 << {i};",
                    flag.name.to_shouty_snake_case(),
                );
            }
            uwriteln!(self.src, "}}");
            uwriteln!(self.src, "}}");

            let n = flags.flags.len();
            let n = if n % 8 == 0 { n / 8 } else { n / 8 + 1 };
            uwriteln!(
                self.src,
                r#"
impl<W> {wrpc_transport}::Encode<W> for self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<W> {wrpc_transport}::Encode<W> for &self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<W> {wrpc_transport}::Encode<W> for &&self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<R> {wrpc_transport}::Decode<R> for self::{name} {{
    type Decoder = {mod_name}::Codec;
    type ListDecoder = {wrpc_transport}::SyncCodec<{wasm_tokio}::CoreVecDecoder<Self::Decoder>>;
}}

use {bitflags} as __{mod_name}__bitflags;

mod {mod_name} {{
    #[derive(::core::clone::Clone, ::core::marker::Copy, ::core::fmt::Debug, ::core::default::Default, ::core::cmp::PartialEq, ::core::cmp::Eq)]
    pub struct Codec;

    #[automatically_derived]
    impl<W> {wrpc_transport}::Deferred<W> for Codec {{
        fn take_deferred(&mut self) -> ::core::option::Option<{wrpc_transport}::DeferredFn<W>> {{
            None
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Decoder for Codec {{
        type Item = super::{name};
        type Error = ::std::io::Error;

        fn decode(&mut self, src: &mut {bytes}::BytesMut) -> ::core::result::Result<::core::option::Option<Self::Item>, Self::Error> {{
            let n = src.len();
            if n < {n} {{
                src.reserve({n} - n);
                return Ok(None)
            }}
            let mut bits = <<super::{name} as super::__{mod_name}__bitflags::Flags>::Bits as super::__{mod_name}__bitflags::Bits>::EMPTY.to_le_bytes();
            bits[..{n}].copy_from_slice(&src.split_to({n}));
            if let Some(item) = <super::{name} as super::__{mod_name}__bitflags::Flags>::from_bits(<super::{name} as super::__{mod_name}__bitflags::Flags>::Bits::from_le_bytes(bits)) {{
                Ok(Some(item))
            }} else {{
                Err(::std::io::Error::new(::std::io::ErrorKind::Other, "invalid bits"))
            }}
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            dst.extend_from_slice(&item.bits().to_le_bytes());
            Ok(())
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<&super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: &super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            self.encode(*item, dst)
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<&&super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: &&super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            self.encode(**item, dst)
        }}
    }}
}}"#,
            );
        }
    }

    fn type_variant(
        &mut self,
        id: TypeId,
        ty_name: &str,
        Variant { cases, .. }: &Variant,
        docs: &Docs,
    ) {
        let info = self.info(id);
        // We use a BTree set to make sure we don't have any duplicates and have a stable order
        let additional_derives: BTreeSet<String> = self
            .gen
            .opts
            .additional_derive_attributes
            .iter()
            .cloned()
            .collect();
        if let Some(name) = self.name_of(id) {
            let (paths, _) = async_paths_tyid(self.resolve, id);

            self.rustdoc(docs);
            let mut derives = additional_derives.clone();
            if info.is_copy() && paths.is_empty() {
                derives.extend(
                    ["::core::marker::Copy", "::core::clone::Clone"]
                        .into_iter()
                        .map(std::string::ToString::to_string),
                );
            } else if info.is_clone() && paths.is_empty() {
                derives.insert("::core::clone::Clone".to_string());
            }
            if !derives.is_empty() {
                self.push_str("#[derive(");
                self.push_str(&derives.into_iter().collect::<Vec<_>>().join(", "));
                self.push_str(")]\n");
            }
            uwriteln!(self.src, "pub enum {name} {{");
            for Case { name, ty, .. } in cases {
                self.rustdoc(docs);
                self.push_str(&name.to_upper_camel_case());
                if let Some(ty) = ty {
                    self.push_str("(");
                    self.print_ty(ty, true, false);
                    self.push_str(")");
                }
                self.push_str(",\n");
            }
            self.push_str("}\n");

            self.print_rust_enum_debug(
                &name,
                cases
                    .iter()
                    .map(|Case { name, ty, .. }| (name.to_upper_camel_case(), ty.as_ref())),
            );

            if info.error {
                self.push_str("impl");
                self.push_str(" ::core::fmt::Display for ");
                self.push_str(&name);
                self.push_str(" {\n");
                self.push_str(
                    "fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {\n",
                );
                self.push_str("write!(f, \"{:?}\", self)\n");
                self.push_str("}\n");
                self.push_str("}\n");
                self.push_str("\n");

                self.push_str("impl");
                self.push_str(" ::std::error::Error for ");
                self.push_str(&name);
                self.push_str(" {}\n");
            }

            let bytes = self.gen.bytes_path().to_string();
            let tokio_util = self.gen.tokio_util_path().to_string();
            let wasm_tokio = self.gen.wasm_tokio_path().to_string();
            let wrpc_transport = self.gen.wrpc_transport_path().to_string();

            let mod_name = to_rust_ident(ty_name);

            if cases.iter().all(|Case { ty, .. }| ty.is_none()) {
                uwriteln!(
                    self.src,
                    r#"
impl<W> {wrpc_transport}::Encode<W> for self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<W> {wrpc_transport}::Encode<W> for &self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<W> {wrpc_transport}::Encode<W> for &&self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<R> {wrpc_transport}::Decode<R> for self::{name} {{
    type Decoder = {mod_name}::Codec;
    type ListDecoder = {wrpc_transport}::SyncCodec<{wasm_tokio}::CoreVecDecoder<Self::Decoder>>;
}}

mod {mod_name} {{
    #[derive(::core::clone::Clone, ::core::marker::Copy, ::core::fmt::Debug, ::core::default::Default, ::core::cmp::PartialEq, ::core::cmp::Eq)]
    pub struct Codec;

    #[automatically_derived]
    impl<W> {wrpc_transport}::Deferred<W> for Codec {{
        fn take_deferred(&mut self) -> ::core::option::Option<{wrpc_transport}::DeferredFn<W>> {{
            None
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Decoder for Codec {{
        type Item = super::{name};
        type Error = ::std::io::Error;

        fn decode(&mut self, src: &mut {bytes}::BytesMut) -> ::core::result::Result<::core::option::Option<Self::Item>, Self::Error> {{
            let Some(disc) = {wasm_tokio}::Leb128DecoderU32.decode(src)? else {{
                return Ok(None)
            }};
            match disc {{"#
                );
                for (i, case) in cases.iter().enumerate() {
                    let case = case.name.to_upper_camel_case();
                    uwriteln!(self.src, "{i} => Ok(Some(super::{name}::{case})),");
                }
                uwriteln!(
                    self.src,
                    r#"_ => Err(::std::io::Error::new(::std::io::ErrorKind::InvalidInput, format!("unknown variant discriminant `{{disc}}`"))),
            }}
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            {wasm_tokio}::Leb128Encoder.encode(match item {{"#
                );
                for (i, case) in cases.iter().enumerate() {
                    let case = case.name.to_upper_camel_case();
                    uwriteln!(self.src, "super::{name}::{case} => {i}_u32,");
                }
                uwriteln!(
                    self.src,
                    r#"
            }}, dst)
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<&super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: &super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            self.encode(*item, dst)
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<&&super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: &&super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            self.encode(**item, dst)
        }}
    }}
}}"#,
                );
            } else {
                let tokio = self.gen.tokio_path().to_string();

                let (paths, _) = async_paths_tyid(self.resolve, id);
                if paths.is_empty() {
                    uwrite!(
                        self.src,
                        r#"
impl<W> {wrpc_transport}::Encode<W> for &self::{name}
where
    W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
{{
    type Encoder = {mod_name}::Encoder<W>;
}}

impl<W> {wrpc_transport}::Encode<W> for &&self::{name}
where
    W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
{{
    type Encoder = {mod_name}::Encoder<W>;
}}
"#
                    );
                }

                uwrite!(
                    self.src,
                    r#"
impl<W> {wrpc_transport}::Encode<W> for self::{name}
where
    W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
{{
    type Encoder = {mod_name}::Encoder<W>;
}}

impl<R> {wrpc_transport}::Decode<R> for self::{name}
where
    R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
{{
    type Decoder = {mod_name}::Decoder<R>;
    type ListDecoder = {wrpc_transport}::ListDecoder<Self::Decoder, R>;
}}

mod {mod_name} {{
    pub struct Encoder<W>(::core::option::Option<{wrpc_transport}::DeferredFn<W>>);

    #[automatically_derived]
    impl<W> ::core::default::Default for Encoder<W> {{
        fn default() -> Self {{ 
            Self(None)
        }}
    }}

    #[automatically_derived]
    impl<W> {wrpc_transport}::Deferred<W> for Encoder<W> {{
        fn take_deferred(&mut self) -> ::core::option::Option<{wrpc_transport}::DeferredFn<W>> {{
            self.0.take()
        }}
    }}"#
                );

                let mut names = vec![format!("super::{name}")];
                if paths.is_empty() {
                    names.extend_from_slice(&[
                        format!("&super::{name}"),
                        format!("&&super::{name}"),
                    ]);
                }
                for ty in names {
                    uwrite!(
                        self.src,
                        r#"
    #[automatically_derived]
    impl<W> {tokio_util}::codec::Encoder<{ty}> for Encoder<W> 
    where
        W: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<W> + {tokio}::io::AsyncWrite + ::core::marker::Unpin + 'static,
    {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: {ty}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            match item {{"#
                    );
                    for (
                        i,
                        Case {
                            name: case_name,
                            ty,
                            ..
                        },
                    ) in cases.iter().enumerate()
                    {
                        let case = case_name.to_upper_camel_case();
                        if ty.is_some() {
                            uwrite!(
                                self.src,
                                r"
                super::{name}::{case}(payload) => {{
                    {wasm_tokio}::Leb128Encoder.encode({i}_u32, dst)?;
                    self.0 = {wrpc_transport}::Encode::<W>::encode(payload, &mut ::core::default::Default::default(), dst)?;
                    Ok(())
                }},"
                            );
                        } else {
                            uwrite!(
                                self.src,
                                "
                super::{name}::{case} => {wasm_tokio}::Leb128Encoder.encode({i}_u32, dst),"
                            );
                        }
                    }
                    uwrite!(
                        self.src,
                        r"
            }}
        }}
    }}"
                    );
                }

                uwrite!(
                    self.src,
                    r"
    pub enum PayloadDecoder<R>
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{"
                );

                for Case {
                    name: case_name,
                    ty,
                    ..
                } in cases
                {
                    if let Some(ty) = ty {
                        let case = case_name.to_upper_camel_case();
                        uwrite!(
                            self.src,
                            r"
                {case}(<"
                        );
                        self.print_ty(ty, true, true);
                        uwriteln!(self.src, " as {wrpc_transport}::Decode<R>>::Decoder),");
                    }
                }

                uwrite!(
                    self.src,
                    r#"
    }}

    pub enum Decoder<R>
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{
        Payload(::core::option::Option<PayloadDecoder<R>>),
        Deferred(::core::option::Option<{wrpc_transport}::DeferredFn<R>>)
    }}

    #[automatically_derived]
    impl<R> ::core::default::Default for Decoder<R>
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{
        fn default() -> Self {{
            Self::Payload(None)
        }}
    }}

    #[automatically_derived]
    impl<R> {wrpc_transport}::Deferred<R> for Decoder<R>
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{
        fn take_deferred(&mut self) -> ::core::option::Option<{wrpc_transport}::DeferredFn<R>> {{
            match self {{
                Self::Payload(None) => None,"#
                );
                for Case {
                    name: case_name,
                    ty,
                    ..
                } in cases
                {
                    let case = case_name.to_upper_camel_case();
                    if ty.is_some() {
                        uwrite!(
                            self.src,
                            r"
                Self::Payload(Some(PayloadDecoder::{case}(ref mut dec))) => dec.take_deferred(),"
                        );
                    }
                }
                uwriteln!(
                    self.src,
                    r#"
                Self::Deferred(f) => {{
                    let f = f.take();
                    *self = Self::Payload(None);
                    f
                }},
            }}
        }}
    }}

    #[automatically_derived]
    impl<R> {tokio_util}::codec::Decoder for Decoder<R> 
    where
        R: ::core::marker::Send + ::core::marker::Sync + {wrpc_transport}::Index<R> + {tokio}::io::AsyncRead + ::core::marker::Unpin + 'static,
    {{
        type Item = super::{name};
        type Error = ::std::io::Error;

        fn decode(&mut self, src: &mut {bytes}::BytesMut) -> ::core::result::Result<::core::option::Option<Self::Item>, Self::Error> {{
            let state = if let Self::Payload(Some(ref mut state)) = self {{
                state
            }} else {{
                let Some(disc) = {wasm_tokio}::Leb128DecoderU32.decode(src)? else {{
                    return Ok(None)
                }};
                match disc {{"#
                );
                for (
                    i,
                    Case {
                        name: case_name,
                        ty,
                        ..
                    },
                ) in cases.iter().enumerate()
                {
                    let case = case_name.to_upper_camel_case();
                    if ty.is_some() {
                        uwrite!(
                            self.src,
                            r"
                    {i} => {{
                        *self = Self::Payload(::core::option::Option::default());
                        let Self::Payload(ref mut dec) = self else {{
                            unreachable!()
                        }};
                        dec.insert(PayloadDecoder::{case}(::core::default::Default::default()))
                    }},"
                        );
                    } else {
                        uwrite!(
                            self.src,
                            "
                    {i} => return Ok(Some(super::{name}::{case})),"
                        );
                    }
                }
                uwrite!(
                    self.src,
                    r#"
                    _ => return Err(::std::io::Error::new(::std::io::ErrorKind::InvalidInput, format!("unknown variant discriminant `{{disc}}`"))),
                }}
            }};

            match state {{"#
                );
                for Case {
                    name: case_name,
                    ty,
                    ..
                } in cases
                {
                    let case = case_name.to_upper_camel_case();
                    if ty.is_some() {
                        uwriteln!(
                            self.src,
                            r"
                        PayloadDecoder::{case}(dec) => {{
                            let Some(payload) = dec.decode(src)? else {{
                                return Ok(None)
                            }};
                            *self = Self::Deferred({wrpc_transport}::Deferred::<R>::take_deferred(dec));
                            Ok(Some(super::{name}::{case}(payload)))
                        }},"
                        );
                    }
                }
                uwriteln!(
                    self.src,
                    r#"
            }}
        }}
    }}
}}"#,
                );
            }
        }
    }

    fn type_option(&mut self, id: TypeId, _name: &str, payload: &Type, docs: &Docs) {
        if let Some(name) = self.name_of(id) {
            self.rustdoc(docs);
            uwrite!(self.src, "pub type {name} = ");
            self.print_option(payload, true, false);
            self.push_str(";\n");
        }
    }

    fn type_result(&mut self, id: TypeId, _name: &str, result: &Result_, docs: &Docs) {
        if let Some(name) = self.name_of(id) {
            self.rustdoc(docs);
            uwrite!(self.src, "pub type {name} = ");
            self.print_result(result, true, false);
            self.push_str(";\n");
        }
    }

    fn type_enum(&mut self, id: TypeId, ty_name: &str, enum_: &Enum, docs: &Docs) {
        let info = self.info(id);

        if let Some(name) = self.name_of(id) {
            self.rustdoc(docs);
            self.push_str("#[repr(");
            self.int_repr(enum_.tag());
            self.push_str(")]\n");
            // We use a BTree set to make sure we don't have any duplicates and a stable order
            let mut derives: BTreeSet<String> = self
                .gen
                .opts
                .additional_derive_attributes
                .iter()
                .cloned()
                .collect();
            derives.extend(
                [
                    ":: core :: clone :: Clone",
                    ":: core :: marker :: Copy",
                    ":: core :: cmp :: PartialEq",
                    ":: core :: cmp :: Eq",
                ]
                .into_iter()
                .map(std::string::ToString::to_string),
            );
            self.push_str("#[derive(");
            self.push_str(&derives.into_iter().collect::<Vec<_>>().join(", "));
            self.push_str(")]\n");
            uwriteln!(self.src, "pub enum {name} {{");
            for case in &enum_.cases {
                self.rustdoc(&case.docs);
                self.push_str(&case.name.to_upper_camel_case());
                self.push_str(",\n");
            }
            self.push_str("}\n");

            // Auto-synthesize an implementation of the standard `Error` trait for
            // error-looking types based on their name.
            if info.error {
                self.push_str("impl ");
                self.push_str(&name);
                self.push_str("{\n");

                self.push_str("pub fn name(&self) -> &'static str {\n");
                self.push_str("match self {\n");
                for case in &enum_.cases {
                    self.push_str(&name);
                    self.push_str("::");
                    self.push_str(&case.name.to_upper_camel_case());
                    self.push_str(" => \"");
                    self.push_str(case.name.as_str());
                    self.push_str("\",\n");
                }
                self.push_str("}\n");
                self.push_str("}\n");

                self.push_str("pub fn message(&self) -> &'static str {\n");
                self.push_str("match self {\n");
                for case in &enum_.cases {
                    self.push_str(&name);
                    self.push_str("::");
                    self.push_str(&case.name.to_upper_camel_case());
                    self.push_str(" => \"");
                    if let Some(contents) = &case.docs.contents {
                        self.push_str(contents.trim());
                    }
                    self.push_str("\",\n");
                }
                self.push_str("}\n");
                self.push_str("}\n");

                self.push_str("}\n");

                self.push_str("impl ::core::fmt::Debug for ");
                self.push_str(&name);
                self.push_str(
                "{\nfn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {\n",
            );
                self.push_str("f.debug_struct(\"");
                self.push_str(&name);
                self.push_str("\")\n");
                self.push_str(".field(\"code\", &(*self as i32))\n");
                self.push_str(".field(\"name\", &self.name())\n");
                self.push_str(".field(\"message\", &self.message())\n");
                self.push_str(".finish()\n");
                self.push_str("}\n");
                self.push_str("}\n");

                self.push_str("impl ::core::fmt::Display for ");
                self.push_str(&name);
                self.push_str(
                "{\nfn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {\n",
            );
                self.push_str("write!(f, \"{} (error {})\", self.name(), *self as i32)\n");
                self.push_str("}\n");
                self.push_str("}\n");
                self.push_str("\n");
                self.push_str("impl ::std::error::Error for ");
                self.push_str(&name);
                self.push_str(" {}\n");
            } else {
                self.print_rust_enum_debug(
                    &name,
                    enum_
                        .cases
                        .iter()
                        .map(|c| (c.name.to_upper_camel_case(), None)),
                );
            }

            let bytes = self.gen.bytes_path().to_string();
            let tokio_util = self.gen.tokio_util_path().to_string();
            let wasm_tokio = self.gen.wasm_tokio_path().to_string();
            let wrpc_transport = self.gen.wrpc_transport_path().to_string();

            let mod_name = to_rust_ident(ty_name);

            uwriteln!(
                self.src,
                r#"
impl<W> {wrpc_transport}::Encode<W> for self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<W> {wrpc_transport}::Encode<W> for &self::{name} {{
    type Encoder = {mod_name}::Codec;
}}

impl<R> {wrpc_transport}::Decode<R> for self::{name} {{
    type Decoder = {mod_name}::Codec;
    type ListDecoder = {wrpc_transport}::SyncCodec<{wasm_tokio}::CoreVecDecoder<Self::Decoder>>;
}}

mod {mod_name} {{
    #[derive(::core::clone::Clone, ::core::marker::Copy, ::core::fmt::Debug, ::core::default::Default, ::core::cmp::PartialEq, ::core::cmp::Eq)]
    pub struct Codec;

    #[automatically_derived]
    impl<W> {wrpc_transport}::Deferred<W> for Codec {{
        fn take_deferred(&mut self) -> ::core::option::Option<{wrpc_transport}::DeferredFn<W>> {{
            None
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Decoder for Codec {{
        type Item = super::{name};
        type Error = ::std::io::Error;

        fn decode(&mut self, src: &mut {bytes}::BytesMut) -> ::core::result::Result<::core::option::Option<Self::Item>, Self::Error> {{
            let Some(disc) = {wasm_tokio}::Leb128DecoderU32.decode(src)? else {{
                return Ok(None)
            }};
            match disc {{"#
            );
            for (i, case) in enum_.cases.iter().enumerate() {
                let case = case.name.to_upper_camel_case();
                uwriteln!(self.src, "{i} => Ok(Some(super::{name}::{case})),");
            }
            uwriteln!(
                self.src,
                r#"_ => Err(::std::io::Error::new(::std::io::ErrorKind::InvalidInput, format!("unknown enum discriminant `{{disc}}`"))),
            }}
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            {wasm_tokio}::Leb128Encoder.encode(match item {{"#
            );
            for (i, case) in enum_.cases.iter().enumerate() {
                let case = case.name.to_upper_camel_case();
                uwriteln!(self.src, "super::{name}::{case} => {i}_u32,");
            }
            uwriteln!(
                self.src,
                r#"
            }}, dst)
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<&super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: &super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            self.encode(*item, dst)
        }}
    }}

    #[automatically_derived]
    impl {tokio_util}::codec::Encoder<&&super::{name}> for Codec {{
        type Error = ::std::io::Error;

        fn encode(&mut self, item: &&super::{name}, dst: &mut {bytes}::BytesMut) -> ::core::result::Result<(), Self::Error> {{
            self.encode(**item, dst)
        }}
    }}
}}"#,
            );
        }
    }

    fn type_alias(&mut self, id: TypeId, _name: &str, ty: &Type, docs: &Docs) {
        if let Some(name) = self.name_of(id) {
            self.rustdoc(docs);
            uwrite!(self.src, "pub type {name} = ");
            self.print_ty(ty, true, false);
            self.push_str(";\n");
        }
    }

    fn type_list(&mut self, id: TypeId, _name: &str, ty: &Type, docs: &Docs) {
        if let Some(name) = self.name_of(id) {
            self.rustdoc(docs);
            uwrite!(self.src, "pub type {name} = ");
            self.print_list(ty, true, false);
            self.push_str(";\n");
        }
    }

    fn type_builtin(&mut self, _id: TypeId, name: &str, ty: &Type, docs: &Docs) {
        self.rustdoc(docs);
        uwrite!(self.src, "pub type {} = ", name.to_upper_camel_case());
        self.print_ty(ty, true, false);
        self.src.push_str(";\n");
    }
}
