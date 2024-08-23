//go:generate $WIT_BINDGEN_WRPC go --gofmt=false --world sync-server --out-dir bindings/sync_server --package gowrpc.lol/tests/go/bindings/sync_server ../wit

package integration

import (
	"context"
	"fmt"
	"log/slog"

	wrpc "gowrpc.lol/go"
	"gowrpc.lol/tests/go/bindings/sync_server/exports/wrpc_test/integration/sync"
)

type SyncHandler struct{}

func (SyncHandler) F(ctx context.Context, x string) (uint32, error) {
	slog.DebugContext(ctx, "handling `f`", "x", x)
	if x != "f" {
		panic(fmt.Errorf("expected: `f`\ngot:`%s`", x))
	}
	return 42, nil
}

func (SyncHandler) Foo(ctx context.Context, x string) error {
	slog.DebugContext(ctx, "handling `foo`", "x", x)
	if x != "foo" {
		panic(fmt.Errorf("expected: `foo`\ngot:`%s`", x))
	}
	return nil
}

func (SyncHandler) Fallible(ctx context.Context, ok bool) (*wrpc.Result[bool, string], error) {
	slog.DebugContext(ctx, "handling `fallible`", "ok", ok)
	if ok {
		return wrpc.Ok[string](true), nil
	} else {
		return wrpc.Err[bool]("test"), nil
	}
}

func (SyncHandler) Numbers(ctx context.Context) (*wrpc.Tuple10[uint8, uint16, uint32, uint64, int8, int16, int32, int64, float32, float64], error) {
	return &wrpc.Tuple10[uint8, uint16, uint32, uint64, int8, int16, int32, int64, float32, float64]{
		V0: 1, V1: 2, V2: 3, V3: 4, V4: 5, V5: 6, V6: 7, V7: 8, V8: 9, V9: 10,
	}, nil
}

func (SyncHandler) WithFlags(ctx context.Context, a, b, c bool) (*sync.Abc, error) {
	slog.DebugContext(ctx, "handling `with-flags`", "a", a, "b", b, "c", c)
	return &sync.Abc{A: a, B: b, C: c}, nil
}

func (SyncHandler) WithVariantList(ctx context.Context) ([]*sync.Var, error) {
	slog.DebugContext(ctx, "handling `with-variant-list`")
	return []*sync.Var{
		sync.NewVarEmpty(),
		sync.NewVarVar(&sync.Rec{
			Nested: &sync.RecNested{
				Foo: "foo",
			},
		}),
		sync.NewVarEmpty(),
		sync.NewVarEmpty(),
		sync.NewVarEmpty(),
		sync.NewVarVar(&sync.Rec{
			Nested: &sync.RecNested{
				Foo: "bar",
			},
		}),
	}, nil
}

func (SyncHandler) WithVariantOption(ctx context.Context, ok bool) (*sync.Var, error) {
	slog.DebugContext(ctx, "handling `with-variant-option`", "ok", ok)
	if ok {
		return sync.NewVarVar(&sync.Rec{
			Nested: &sync.RecNested{
				Foo: "bar",
			},
		}), nil
	}
	return nil, nil
}

func (SyncHandler) WithRecord(ctx context.Context) (*sync.Rec, error) {
	slog.DebugContext(ctx, "handling `with-record`")
	return &sync.Rec{
		Nested: &sync.RecNested{
			Foo: "foo",
		},
	}, nil
}

func (SyncHandler) WithRecordList(ctx context.Context, n uint8) ([]*sync.Rec, error) {
	slog.DebugContext(ctx, "handling `with-record-list`", "n", n)
	if n == 0 {
		return nil, nil
	}
	vs := make([]*sync.Rec, n)
	for i := range vs {
		vs[i] = &sync.Rec{
			Nested: &sync.RecNested{
				Foo: fmt.Sprintf("%d", i),
			},
		}
	}
	return vs, nil
}

func (SyncHandler) WithRecordTuple(ctx context.Context) (*wrpc.Tuple2[*sync.Rec, *sync.Rec], error) {
	slog.DebugContext(ctx, "handling `with-record-tuple`")
	return &wrpc.Tuple2[*sync.Rec, *sync.Rec]{
		V0: &sync.Rec{
			Nested: &sync.RecNested{
				Foo: "0",
			},
		},
		V1: &sync.Rec{
			Nested: &sync.RecNested{
				Foo: "1",
			},
		},
	}, nil
}

func (SyncHandler) WithEnum(ctx context.Context) (sync.Foobar, error) {
	return sync.Foobar_Bar, nil
}
