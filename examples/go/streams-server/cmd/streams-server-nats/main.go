package main

import (
	"context"
	"fmt"
	"log"
	"log/slog"
	"os"
	"os/signal"
	"syscall"

	server "github.com/bytecodealliance/wrpc/examples/go/streams-server/bindings"
	"github.com/bytecodealliance/wrpc/examples/go/streams-server/bindings/exports/wrpc_examples/streams/handler"
	wrpc "gowrpc.lol/go"
	wrpcnats "gowrpc.lol/go/nats"
	"github.com/nats-io/nats.go"
)

type Handler struct{}

func (Handler) Echo(ctx context.Context, req *handler.Req) (wrpc.ReceiveCompleter[[]uint64], wrpc.ReadCompleter, error) {
	slog.InfoContext(ctx, "handling `wrpc-examples:streams/handler.echo`")
	return req.Numbers, req.Bytes, nil
}

func run() (err error) {
	nc, err := nats.Connect(nats.DefaultURL)
	if err != nil {
		return fmt.Errorf("failed to connect to NATS.io: %w", err)
	}
	defer nc.Close()
	defer func() {
		if dErr := nc.Drain(); dErr != nil {
			if err == nil {
				err = fmt.Errorf("failed to drain NATS.io connection: %w", dErr)
			} else {
				slog.Error("failed to drain NATS.io connection", "err", dErr)
			}
		}
	}()

	client := wrpcnats.NewClient(nc, wrpcnats.WithPrefix("go"))
	stop, err := server.Serve(client, Handler{})
	if err != nil {
		return fmt.Errorf("failed to serve `server` world: %w", err)
	}

	signalCh := make(chan os.Signal, 1)
	signal.Notify(signalCh, syscall.SIGINT)
	<-signalCh

	if err = stop(); err != nil {
		return fmt.Errorf("failed to stop `server` world: %w", err)
	}
	return nil
}

func init() {
	slog.SetDefault(slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{
		Level: slog.LevelInfo, ReplaceAttr: func(groups []string, a slog.Attr) slog.Attr {
			if a.Key == slog.TimeKey {
				return slog.Attr{}
			}
			return a
		},
	})))
}

func main() {
	if err := run(); err != nil {
		log.Fatal(err)
	}
}
