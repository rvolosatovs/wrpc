module github.com/bytecodealliance/wrpc/examples/go/streams-client

go 1.22.2

require (
	gowrpc.lol/go v0.0.1
	github.com/nats-io/nats.go v1.37.0
)

require (
	github.com/klauspost/compress v1.17.8 // indirect
	github.com/nats-io/nkeys v0.4.7 // indirect
	github.com/nats-io/nuid v1.0.1 // indirect
	golang.org/x/crypto v0.23.0 // indirect
	golang.org/x/sys v0.20.0 // indirect
)

replace gowrpc.lol/go v0.0.1 => ../../../go
