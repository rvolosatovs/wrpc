//go:generate $WIT_BINDGEN_WRPC go --world resources-client --out-dir bindings/resources_client --package gowrpc.lol/tests/go/bindings/resources_client ../wit

package integration_test

import (
	"context"
	"log/slog"
	"testing"
	"time"

	wrpc "gowrpc.lol/go"
	wrpcnats "gowrpc.lol/go/nats"
	integration "gowrpc.lol/tests/go"
	"gowrpc.lol/tests/go/bindings/resources_client/strange"
	"gowrpc.lol/tests/go/bindings/resources_client/wrpc_test/integration/resources"
	"gowrpc.lol/tests/go/bindings/resources_server"
	"gowrpc.lol/tests/go/internal"
	"github.com/nats-io/nats.go"
)

func TestResources(t *testing.T) {
	natsSrv := internal.RunNats(t)
	nc, err := nats.Connect(natsSrv.ClientURL())
	if err != nil {
		t.Errorf("failed to connect to NATS.io: %s", err)
		return
	}
	defer nc.Close()
	defer func() {
		if err := nc.Drain(); err != nil {
			t.Errorf("failed to drain NATS.io connection: %s", err)
			return
		}
	}()
	client := wrpcnats.NewClient(nc, wrpcnats.WithPrefix("go"))

	stop, err := resources_server.Serve(client, &integration.ResourcesHandler{}, integration.ResourcesStrangeHandler{})
	if err != nil {
		t.Errorf("failed to serve `resources-server` world: %s", err)
		return
	}

	var cancel func()
	ctx := context.Background()
	dl, ok := t.Deadline()
	if ok {
		ctx, cancel = context.WithDeadline(ctx, dl)
	} else {
		ctx, cancel = context.WithTimeout(ctx, time.Minute)
	}
	defer cancel()

	var foo wrpc.Own[resources.Foo]
	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/resources.[constructor]foo`")
		v, err := resources.NewFoo(ctx, client)
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/resources.[constructor]foo`: %s", err)
			return
		}
		foo = v
	}
	// TODO: Reenable once resource dropping is supported
	//if err := foo.Drop(ctx, client); err != nil {
	//	t.Errorf("failed to drop `foo` resource: %s", err)
	//	return
	//}

	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/resources.[constructor]foo`")
		v, err := resources.NewFoo(ctx, client)
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/resources.[constructor]foo`: %s", err)
			return
		}
		foo = v
	}

	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/resources.[method]foo.bar`")
		v, err := resources.Foo_Bar(ctx, client, foo.Borrow())
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/resources.[method]foo.bar`: %s", err)
			return
		}
		if v != "bar" {
			t.Errorf("expected: `bar`, got: %s", v)
			return
		}
	}

	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/resources.bar`")
		v, err := resources.Bar(ctx, client, foo.Borrow())
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/resources.bar`: %s", err)
			return
		}
		if v != "bar" {
			t.Errorf("expected: `bar`, got: %s", v)
			return
		}
	}

	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/strange.bar`")
		v, err := strange.Bar(ctx, client, foo.Borrow())
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/strange.bar`: %s", err)
			return
		}
		if v != 42 {
			t.Errorf("expected: `bar`, got: %v", v)
			return
		}
	}

	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/resources.[static]foo.foo`")
		v, err := resources.Foo_Foo(ctx, client, foo)
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/resources.bar`: %s", err)
			return
		}
		if v != "foo" {
			t.Errorf("expected: `foo`, got: %s", v)
			return
		}
	}

	// TODO: Reenable once resource dropping is supported
	//if err := foo.Drop(ctx, client); err == nil {
	//	t.Errorf("`foo` resource did not get dropped by moving")
	//	return
	//}

	if err = stop(); err != nil {
		t.Errorf("failed to stop serving `resources-server` world: %s", err)
		return
	}
}
