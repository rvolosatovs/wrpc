use anyhow::{ensure, Context};
use tokio::process::Command;


mod common;

#[cfg(feature = "nats")]

#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn go_bindgen() -> anyhow::Result<()> {
    use core::time::Duration;

    use anyhow::{anyhow, bail};
    use tokio::fs;
    use tokio::time::sleep;
    use tracing::info;

    if let Err(err) = fs::remove_dir_all("tests/go/bindings").await {
        match err.kind() {
            std::io::ErrorKind::NotFound => {}
            _ => bail!(anyhow!(err).context("failed to remove `test/go/bindings`")),
        }
    }
    let status = Command::new("go")
        .current_dir("tests/go")
        .args(["generate", "./..."])
        .env("WIT_BINDGEN_WRPC", env!("CARGO_BIN_EXE_wit-bindgen-wrpc"))
        .kill_on_drop(true)
        .status()
        .await
        .context("failed to call `go generate`")?;
    ensure!(status.success(), "`go generate` failed");

    let status = Command::new("go")
        .current_dir("tests/go")
        .args(["test", "-v", "-failfast"])
        .kill_on_drop(true)
        .status()
        .await
        .context("failed to call `go test`")?;
    ensure!(status.success(), "`go test` failed");

    common::with_nats(|port, nats_client| async move {
        wrpc::generate!({
            world: "sync-client",
            path: "tests/wit",
            additional_derives: [::core::cmp::PartialEq],
        });

        use wrpc_test::integration::sync;
        use wrpc_test::integration::sync::{Abc, Foobar, Rec, RecNested, Var};

        info!("starting `sync-server-nats`");
        let mut server = Command::new("go")
            .current_dir("tests/go")
            .args([
                "run",
                "./cmd/sync-server-nats",
                &format!("nats://localhost:{port}"),
            ])
            .kill_on_drop(true)
            .spawn()
            .context("failed to run `sync-server-nats`")?;

        let client = wrpc_transport_nats::Client::new(nats_client, "go", None);

        // TODO: Remove the need for this
        sleep(Duration::from_secs(1)).await;

        info!("calling `wrpc-test:integration/sync-client.foo.f`");
        let v = foo::f(&client, None, "f")
            .await
            .context("failed to call `wrpc-test:integration/sync-client.foo.f`")?;
        ensure!(v == 42);

        info!("calling `wrpc-test:integration/sync-client.foo.foo`");
        foo::foo(&client, None, "foo")
            .await
            .context("failed to call `wrpc-test:integration/sync-client.foo.foo`")?;

        info!("calling `wrpc-test:integration/sync.fallible`");
        let res = sync::fallible(&client, None, true)
            .await
            .context("failed to call `wrpc-test:integration/sync.fallible`")?;
        ensure!(res == Ok(true));

        info!("calling `wrpc-test:integration/sync.fallible`");
        let res = sync::fallible(&client, None, false)
            .await
            .context("failed to call `wrpc-test:integration/sync.fallible`")?;
        ensure!(res == Err("test".to_string()));

        info!("calling `wrpc-test:integration/sync.numbers`");
        let (a, b, c, d, e, f, g, h, i, j) = sync::numbers(&client, None)
            .await
            .context("failed to call `wrpc-test:integration/sync.numbers`")?;
        ensure!(a == 1);
        ensure!(b == 2);
        ensure!(c == 3);
        ensure!(d == 4);
        ensure!(e == 5);
        ensure!(f == 6);
        ensure!(g == 7);
        ensure!(h == 8);
        ensure!(i == 9.);
        ensure!(j == 10.);

        info!("calling `wrpc-test:integration/sync.with-flags`");
        let v = sync::with_flags(&client, None, true, false, true)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-flags`")?;
        ensure!(v == Abc::A | Abc::C, "{v:?}");

        info!("calling `wrpc-test:integration/sync.with-variant-option`");
        let v = sync::with_variant_option(&client, None, false)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-variant-option`")?;
        ensure!(v.is_none(), "{v:?}");

        info!("calling `wrpc-test:integration/sync.with-variant-option`");
        let v = sync::with_variant_option(&client, None, true)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-variant-option`")?;
        ensure!(
            v == Some(Var::Var(Rec {
                nested: RecNested {
                    foo: "bar".to_string()
                }
            })),
            "{v:?}"
        );

        info!("calling `wrpc-test:integration/sync.with-variant-list`");
        let v = sync::with_variant_list(&client, None)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-variant-list`")?;
        ensure!(
            v == [
                Var::Empty,
                Var::Var(Rec {
                    nested: RecNested {
                        foo: "foo".to_string()
                    }
                }),
                Var::Empty,
                Var::Empty,
                Var::Empty,
                Var::Var(Rec {
                    nested: RecNested {
                        foo: "bar".to_string()
                    }
                }),
            ],
            "{v:?}"
        );

        info!("calling `wrpc-test:integration/sync.with-record`");
        let v = sync::with_record(&client, None)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-record`")?;
        ensure!(
            v == Rec {
                nested: RecNested {
                    foo: "foo".to_string()
                }
            },
            "{v:?}"
        );

        info!("calling `wrpc-test:integration/sync.with-record-list`");
        let v = sync::with_record_list(&client, None, 0)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-record-list`")?;
        ensure!(v.is_empty(), "{v:?}");

        info!("calling `wrpc-test:integration/sync.with-record-list`");
        let v = sync::with_record_list(&client, None, 3)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-record-list`")?;
        ensure!(
            v == [
                Rec {
                    nested: RecNested {
                        foo: "0".to_string()
                    }
                },
                Rec {
                    nested: RecNested {
                        foo: "1".to_string()
                    }
                },
                Rec {
                    nested: RecNested {
                        foo: "2".to_string()
                    }
                },
            ],
            "{v:?}",
        );

        info!("calling `wrpc-test:integration/sync.with-record-tuple`");
        let v = sync::with_record_tuple(&client, None)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-record-tuple`")?;
        ensure!(
            v == (
                Rec {
                    nested: RecNested {
                        foo: "0".to_string()
                    }
                },
                Rec {
                    nested: RecNested {
                        foo: "1".to_string()
                    }
                },
            ),
            "{v:?}",
        );

        info!("calling `wrpc-test:integration/sync.with-enum`");
        let v = sync::with_enum(&client, None)
            .await
            .context("failed to call `wrpc-test:integration/sync.with-enum-tuple`")?;
        ensure!(v == Foobar::Bar, "{v:?}",);

        server
            .start_kill()
            .context("failed to kill `sync-server-nats`")?;
        server
            .wait_with_output()
            .await
            .context("failed to wait for `sync-server-nats` to exit")?;

        Ok(())
    })
    .await
}


#[test_log::test(tokio::test(flavor = "multi_thread"))]
async fn go() -> anyhow::Result<()> {
    let status = Command::new("go")
        .current_dir("go")
        .args(["test", "-v", "-failfast", "./..."])
        .kill_on_drop(true)
        .status()
        .await
        .context("failed to call `go test`")?;
    ensure!(status.success(), "`go test` failed");
    Ok(())
}
