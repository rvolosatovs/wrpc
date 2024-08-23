use std::fs;
use std::path::Path;
use std::process::Command;

macro_rules! codegen_test {
    (issue668 $name:tt $test:tt) => {};
    (multiversion $name:tt $test:tt) => {};
    ($id:ident $name:tt $test:tt) => {
        #[test]
        fn $id() {
            test_helpers::run_world_codegen_test(
                "go",
                $test.as_ref(),
                |resolve, world, files| {
                    wit_bindgen_wrpc_go::Opts {
                        gofmt: false,
                        package: "bindings".to_string(),
                    }
                    .build()
                    .generate(resolve, world, files)
                    .unwrap()
                },
                verify,
            )
        }
    };
}

test_helpers::codegen_tests!();

fn verify(dir: &Path, _name: &str) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let go_work = dir.join("go.work");
    fs::write(
        &go_work,
        r"go 1.22.2
use .",
    )
    .unwrap_or_else(|_| panic!("failed to write `{}`", go_work.display()));
    let go_mod = dir.join("go.mod");
    fs::write(
        &go_mod,
        format!(
            r"module bindings

go 1.22.2

require gowrpc.lol/go v0.0.1

replace gowrpc.lol/go v0.0.1 => {}",
            root.join("go").display(),
        ),
    )
    .unwrap_or_else(|_| panic!("failed to write `{}`", go_mod.display()));

    test_helpers::run_command(Command::new("go").args(["test", "./..."]).current_dir(dir));
}
