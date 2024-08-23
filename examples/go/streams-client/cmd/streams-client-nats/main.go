package main

import (
	"context"
	"fmt"
	"io"
	"log"
	"log/slog"
	"os"
	"sync"
	"time"

	"github.com/bytecodealliance/wrpc/examples/go/streams-client/bindings/wrpc_examples/streams/handler"
	wrpcnats "gowrpc.lol/go/nats"
	"github.com/nats-io/nats.go"
)

type ThrottleStream[T any] struct {
	tick   <-chan time.Time
	values []T
}

func (s *ThrottleStream[T]) IsComplete() bool {
	// The stream has full contents available in `s.values`,
	// but we return `false` to force async transmission
	return false
}

func (s *ThrottleStream[T]) Receive() ([]T, error) {
	if len(s.values) == 0 {
		return nil, io.EOF
	}
	<-s.tick
	var v T
	v, s.values = s.values[0], s.values[1:]
	return []T{v}, nil
}

type ThrottleReader struct {
	tick <-chan time.Time
	buf  []byte
}

func (s *ThrottleReader) IsComplete() bool {
	// The reader has full contents available in `s.buf`,
	// but we return `false` to force async transmission
	return false
}

func (s *ThrottleReader) Read(p []byte) (int, error) {
	if len(s.buf) == 0 {
		return 0, io.EOF
	}
	<-s.tick
	var b byte
	b, s.buf = s.buf[0], s.buf[1:]
	p[0] = b
	return 1, nil
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

	for _, prefix := range os.Args[1:] {
		client := wrpcnats.NewClient(nc, wrpcnats.WithPrefix(prefix))
		numbers, bytes, errCh, err := handler.Echo(context.Background(), client, &handler.Req{
			Numbers: &ThrottleStream[uint64]{
				tick:   time.Tick(time.Second),
				values: []uint64{1, 2, 3, 4, 5, 6, 7, 8, 9, 10},
			},
			Bytes: &ThrottleReader{
				tick: time.Tick(time.Second),
				buf:  []byte("foo bar baz"),
			},
		})
		if err != nil {
			return fmt.Errorf("failed to call `wrpc-examples:streams/handler.echo`: %w", err)
		}
		var wg sync.WaitGroup
		wg.Add(1)
		go func() {
			defer wg.Done()
			// defer numbers.Close()
			for {
				chunk, err := numbers.Receive()
				if err == io.EOF {
					return
				}
				if err != nil {
					slog.Error("failed to receive number chunk", "err", err)
					return
				}
				fmt.Printf("numbers: %v\n", chunk)
			}
		}()
		wg.Add(1)
		go func() {
			defer wg.Done()
			// defer bytes.Close()
			var chunk [128]byte
			for {
				n, err := bytes.Read(chunk[:])
				if err == io.EOF {
					return
				}
				if err != nil {
					slog.Error("failed to receive byte chunk", "err", err)
					return
				}
				fmt.Printf("bytes: %s\n", chunk[:n])
			}
		}()
		for err := range errCh {
			slog.Error("failed to transmit async parameters", "err", err)
		}
		wg.Wait()
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
