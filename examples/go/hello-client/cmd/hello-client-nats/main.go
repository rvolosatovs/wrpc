package main

import (
	"context"
	"fmt"
	"log"
	"log/slog"
	"os"

	"github.com/bytecodealliance/wrpc/examples/go/hello-client/bindings/wrpc_examples/hello/handler"
	wrpcnats "gowrpc.lol/go/nats"
	"github.com/nats-io/nats.go"
)

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

	for _, prefix := range os.Args[1:] {
		client := wrpcnats.NewClient(nc, wrpcnats.WithPrefix(prefix))
		greeting, err := handler.Hello(context.Background(), client)
		if err != nil {
			return fmt.Errorf("failed to call `wrpc-examples:hello/handler.hello`: %w", err)
		}
		fmt.Printf("%s: %s\n", prefix, greeting)
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
