// NOTE: This example is a work in-progress and will change significantly going forward

package main

import (
	"fmt"
	"log"
	"os"
	"os/signal"
	"syscall"

	"github.com/nats-io/nats.go"
	"github.com/wrpc/wrpc/go/interface/http/outgoing_handler"
	"github.com/wrpc/wrpc/go/interface/http/types"
	wrpcnats "github.com/wrpc/wrpc/go/nats"
)

func run() error {
	nc, err := nats.Connect(nats.DefaultURL)
	if err != nil {
		return fmt.Errorf("failed to connect to NATS.io: %w", err)
	}
	defer nc.Close()

	stop, err := outgoing_handler.ServeHandle(wrpcnats.NewClient(nc, "go"), func(req *types.RecordRequest) error {
		// TODO: This will eventually be able to return a response
		log.Printf("Request: %#v", req)
		return nil
	})
	if err != nil {
		return err
	}

	signalCh := make(chan os.Signal, 1)
	signal.Notify(signalCh, syscall.SIGINT)
	<-signalCh
	if err = stop(); err != nil {
		log.Printf("failed to stop serving: %s", err)
	}
	if err = nc.Drain(); err != nil {
		log.Printf("failed to drain NATS.io connection: %s", err)
	}
	return nil
}

func init() {
	log.SetFlags(0)
	log.SetOutput(os.Stderr)
}

func main() {
	if err := run(); err != nil {
		log.Fatal(err)
	}
}
