//go:generate $WIT_BINDGEN_WRPC go --world async-client --out-dir bindings/async_client --package gowrpc.lol/tests/go/bindings/async_client ../wit

package integration_test

import (
	"context"
	"io"
	"log/slog"
	"reflect"
	"testing"
	"time"

	wrpcnats "gowrpc.lol/go/nats"
	integration "gowrpc.lol/tests/go"
	"gowrpc.lol/tests/go/bindings/async_client/wrpc_test/integration/async"
	"gowrpc.lol/tests/go/bindings/async_server"
	"gowrpc.lol/tests/go/internal"
	"github.com/nats-io/nats.go"
)

func TestAsync(t *testing.T) {
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

	stop, err := async_server.Serve(client, integration.AsyncHandler{})
	if err != nil {
		t.Errorf("failed to serve `async-server` world: %s", err)
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

	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/async.with-streams`")
		byteRx, stringListRx, err := async.WithStreams(ctx, client, true)
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/async.with-streams`: %s", err)
			return
		}
		b, err := io.ReadAll(byteRx)
		if err != nil {
			t.Errorf("failed to read from stream: %s", err)
			return
		}
		if string(b) != "test" {
			t.Errorf("expected: `test`, got: %s", string(b))
			return
		}
		// TODO: Close
		//if err := byteRx.Close(); err != nil {
		//	t.Errorf("failed to close byte reader: %s", err)
		//	return
		//}

		ss, err := stringListRx.Receive()
		if err != nil {
			t.Errorf("failed to receive ready list<string> stream: %s", err)
			return
		}
		expected := [][]string{{"foo", "bar"}, {"baz"}}
		if !reflect.DeepEqual(ss, expected) {
			t.Errorf("expected: `%#v`, got: %#v", expected, ss)
			return
		}
		ss, err = stringListRx.Receive()
		if ss != nil || err != io.EOF {
			t.Errorf("ready list<string> should have returned (nil, io.EOF), got: (%#v, %v)", ss, err)
			return
		}
		// TODO: Close
		//if err := stringListRx.Close(); err != nil {
		//	t.Errorf("failed to close string list receiver: %s", err)
		//	return
		//}
	}

	{
		slog.DebugContext(ctx, "calling `wrpc-test:integration/async.with-streams`")
		byteRx, stringListRx, err := async.WithStreams(ctx, client, false)
		if err != nil {
			t.Errorf("failed to call `wrpc-test:integration/async.with-streams`: %s", err)
			return
		}
		b, err := io.ReadAll(byteRx)
		if err != nil {
			t.Errorf("failed to read from stream: %s", err)
			return
		}
		if string(b) != "test" {
			t.Errorf("expected: `test`, got: %s", string(b))
			return
		}
		// TODO: Close
		//if err := byteRx.Close(); err != nil {
		//	t.Errorf("failed to close byte reader: %s", err)
		//	return
		//}

		ss, err := stringListRx.Receive()
		if err != nil {
			t.Errorf("failed to receive ready list<string> stream: %s", err)
			return
		}
		expected := [][]string{{"foo", "bar"}, {"baz"}}
		if !reflect.DeepEqual(ss, expected) {
			t.Errorf("expected: `%#v`, got: %#v", expected, ss)
			return
		}
		ss, err = stringListRx.Receive()
		if ss != nil || err != io.EOF {
			t.Errorf("ready list<string> should have returned (nil, io.EOF), got: (%#v, %v)", ss, err)
			return
		}
		// TODO: Close
		//if err := stringListRx.Close(); err != nil {
		//	t.Errorf("failed to close string list receiver: %s", err)
		//	return
		//}
	}

	if err = stop(); err != nil {
		t.Errorf("failed to stop serving `async-server` world: %s", err)
		return
	}
}
