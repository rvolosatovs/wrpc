package wrpc

import "context"

type Transmitter interface {
	Transmit(context.Context, []uint32, []byte) error
}

type Subscriber interface {
	Subscribe(func(context.Context, []byte)) (func() error, error)
	SubscribePath([]*uint32, chan<- error, func(context.Context, []uint32, []byte)) (func() error, error)
}

type ErrorSubscriber interface {
	SubscribeError(func(context.Context, []byte)) (func() error, error)
}

type IncomingInvocation interface {
	Subscriber
	ErrorSubscriber

	Accept(context.Context, []byte) error
}

type OutgoingInvocation interface {
	Subscriber
	ErrorSubscriber

	Invoke(context.Context, []byte, func(context.Context, []byte)) (func() error, Transmitter, error)
}

type Client interface {
	NewInvocation(instance string, name string) OutgoingInvocation

	Serve(instance string, name string, f func(context.Context, []byte, Transmitter, IncomingInvocation) error) (func() error, error)
}
