//go:generate $WIT_BINDGEN_WRPC go --world resources-server --out-dir bindings/resources_server --package gowrpc.lol/tests/go/bindings/resources_server ../wit

package integration

import (
	"context"
	"fmt"
	"sync"

	wrpc "gowrpc.lol/go"
	"gowrpc.lol/tests/go/bindings/resources_server/exports/wrpc_test/integration/resources"
	"github.com/google/uuid"
)

type Foo struct {
	cancel context.CancelFunc
	id     uuid.UUID
}

func (Foo) Bar(ctx context.Context) (string, error) {
	return "bar", nil
}

type ResourcesHandler struct {
	sync.Map
}

func (h *ResourcesHandler) Foo(ctx context.Context) (wrpc.Own[resources.Foo], error) {
	id, err := uuid.NewV7()
	if err != nil {
		return "", fmt.Errorf("failed to generate UUIDv7: %w", err)
	}
	ctx, cancel := context.WithCancel(ctx)
	v := Foo{id: id, cancel: cancel}
	h.Store(id.String(), v)
	go func() {
		<-ctx.Done()
		h.Delete(id)
	}()
	return wrpc.Own[resources.Foo](id.String()), nil
}

func (h *ResourcesHandler) Foo_Foo(ctx context.Context, v wrpc.Own[resources.Foo]) (string, error) {
	stored, ok := h.Load(string(v))
	if !ok {
		return "", fmt.Errorf("unknown resource ID `%s`", string(v))
	}
	foo := stored.(Foo)
	foo.cancel()
	return "foo", nil
}

func (h *ResourcesHandler) Foo_Bar(ctx context.Context, v wrpc.Borrow[resources.Foo]) (string, error) {
	stored, ok := h.Load(string(v))
	if !ok {
		return "", fmt.Errorf("unknown resource ID `%s`", string(v))
	}
	return stored.(Foo).Bar(ctx)
}

func (h *ResourcesHandler) Bar(ctx context.Context, v wrpc.Borrow[resources.Foo]) (string, error) {
	stored, ok := h.Load(string(v))
	if !ok {
		return "", fmt.Errorf("unknown resource ID `%s`", string(v))
	}
	return stored.(Foo).Bar(ctx)
}

type ResourcesStrangeHandler struct{}

func (h ResourcesStrangeHandler) Bar(ctx context.Context, v wrpc.Borrow[resources.Foo]) (uint64, error) {
	return 42, nil
}
