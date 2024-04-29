package wrpcnats

import (
	"context"
	"errors"
	"fmt"
	"log"
	"strconv"
	"strings"

	"github.com/nats-io/nats.go"
	wrpc "github.com/wrpc/wrpc/go"
)

type headerKey struct{}

func HeaderFromContext(ctx context.Context) (nats.Header, bool) {
	v, ok := ctx.Value(headerKey{}).(nats.Header)
	return v, ok
}

func ContextWithHeader(ctx context.Context, header nats.Header) context.Context {
	return context.WithValue(ctx, headerKey{}, header)
}

func errorSubject(prefix string) string {
	return fmt.Sprintf("%s.error", prefix)
}

func paramSubject(prefix string) string {
	return fmt.Sprintf("%s.params", prefix)
}

func resultSubject(prefix string) string {
	return fmt.Sprintf("%s.results", prefix)
}

func invocationSubject(prefix string, instance string, name string) string {
	subject := fmt.Sprintf("wrpc.0.0.1.%s.%s", instance, name)
	if prefix != "" {
		return fmt.Sprintf("%s.%s", prefix, subject)
	}
	return subject
}

func subscribeElement(conn *nats.Conn, prefix string, path []*uint32, errCh chan<- error, f func(context.Context, []uint32, []byte)) (*nats.Subscription, error) {
	subject := prefix
	for _, p := range path {
		if p == nil {
			subject = fmt.Sprintf("%s.*", subject)
		} else {
			subject = fmt.Sprintf("%s.%d", subject, p)
		}
	}
	return conn.Subscribe(subject, func(m *nats.Msg) {
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)
		path := []uint32(nil)
		if len(path) > 0 {
			path = make([]uint32, 0, len(path))
			ps := strings.Split(strings.TrimPrefix(m.Subject, prefix), ".")
			if len(ps) != len(path) {
				errCh <- fmt.Errorf("invalid amount of path elements, expected %d, got %d", len(path), len(ps))
				return
			}
			for _, p := range ps {
				i, err := strconv.ParseUint(p, 10, 32)
				if err != nil {
					errCh <- fmt.Errorf("failed to parse %s as uint32: %w", p, err)
					return
				}
				path = append(path, uint32(i))
			}
		}
		f(ctx, path, m.Data)
	})
}

func transmit(ctx context.Context, conn *nats.Conn, subject string, reply string, buf []byte) error {
	header, hasHeader := HeaderFromContext(ctx)
	m := nats.NewMsg(subject)
	m.Reply = reply
	if hasHeader {
		m.Header = header
	}

	maxPayload := conn.MaxPayload()
	mSize := int64(m.Size())
	if mSize > maxPayload {
		return fmt.Errorf("message size %d is larger than maximum allowed payload size %d", mSize, maxPayload)
	}
	maxPayload -= mSize
	maxPayload = min(maxPayload, int64(len(buf)))
	m.Data, buf = buf[:maxPayload], buf[maxPayload:]
	if err := conn.PublishMsg(m); err != nil {
		return fmt.Errorf("failed to send initial payload chunk: %w", err)
	}
	for len(buf) > 0 {
		m := nats.NewMsg(subject)
		m.Reply = reply
		if hasHeader {
			m.Header = header
		}
		maxPayload = min(maxPayload, int64(len(buf)))
		m.Data, buf = buf[:maxPayload], buf[maxPayload:]
		if err := conn.PublishMsg(m); err != nil {
			return fmt.Errorf("failed to send payload chunk: %w", err)
		}
	}
	return nil
}

type invocation struct {
	conn *nats.Conn
	rx   string
	tx   string
}

func (c *invocation) SubscribeError(f func(context.Context, []byte)) (func() error, error) {
	sub, err := c.conn.Subscribe(fmt.Sprintf("%s.error", c.rx), func(m *nats.Msg) {
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)
		f(ctx, m.Data)
	})
	if err != nil {
		return nil, fmt.Errorf("failed to subscribe for error: %w", err)
	}
	return sub.Unsubscribe, nil
}

type OutgoingInvocation struct{ invocation }

func (c *Client) NewInvocation(instance string, name string) wrpc.OutgoingInvocation {
	return &OutgoingInvocation{
		invocation: invocation{
			conn: c.conn,
			rx:   nats.NewInbox(),
			tx:   invocationSubject(c.prefix, instance, name),
		},
	}
}

func (c *OutgoingInvocation) Subscribe(f func(context.Context, []byte)) (func() error, error) {
	sub, err := c.conn.Subscribe(resultSubject(c.rx), func(m *nats.Msg) {
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)
		f(ctx, m.Data)
	})
	if err != nil {
		return nil, fmt.Errorf("failed to subscribe for results: %w", err)
	}
	return sub.Unsubscribe, nil
}

func (c *OutgoingInvocation) SubscribePath(path []*uint32, errCh chan<- error, f func(context.Context, []uint32, []byte)) (func() error, error) {
	sub, err := subscribeElement(c.conn, resultSubject(c.rx), path, errCh, f)
	if err != nil {
		return nil, fmt.Errorf("failed to subscribe for results at path %v: %w", path, err)
	}
	return sub.Unsubscribe, nil
}

func (c *OutgoingInvocation) SubscribeError(f func(context.Context, []byte)) (func() error, error) {
	return c.invocation.SubscribeError(f)
}

func (c *OutgoingInvocation) Invoke(ctx context.Context, buf []byte, f func(context.Context, []byte)) (func() error, wrpc.Transmitter, error) {
	txCh := make(chan chan<- string, 1)
	sub, err := c.conn.Subscribe(c.rx, func(m *nats.Msg) {
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)
		<-txCh <- m.Reply
		f(ctx, m.Data)
	})
	if err != nil {
		return nil, nil, fmt.Errorf("failed to subscribe for handshake: %w", err)
	}
	if err := transmit(ctx, c.conn, c.tx, "", buf); err != nil {
		return sub.Unsubscribe, nil, fmt.Errorf("failed to transmit handshake: %w", err)
	}
	reply := make(chan string, 1)
	select {
	case txCh <- reply:
		return sub.Unsubscribe, &Transmitter{
			conn:    c.conn,
			subject: <-reply,
		}, nil
	case <-ctx.Done():
		return sub.Unsubscribe, nil, errors.New("handshake timed out")
	}
}

type IncomingInvocation struct{ invocation }

func (c *IncomingInvocation) Subscribe(f func(context.Context, []byte)) (func() error, error) {
	sub, err := c.conn.Subscribe(paramSubject(c.rx), func(m *nats.Msg) {
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)
		f(ctx, m.Data)
	})
	if err != nil {
		return nil, fmt.Errorf("failed to subscribe for parameters: %w", err)
	}
	return sub.Unsubscribe, nil
}

func (c *IncomingInvocation) SubscribePath(path []*uint32, errCh chan<- error, f func(context.Context, []uint32, []byte)) (func() error, error) {
	sub, err := subscribeElement(c.conn, paramSubject(c.rx), path, errCh, f)
	if err != nil {
		return nil, fmt.Errorf("failed to subscribe for parameters at path %v: %w", path, err)
	}
	return sub.Unsubscribe, nil
}

func (c *IncomingInvocation) SubscribeError(f func(context.Context, []byte)) (func() error, error) {
	return c.invocation.SubscribeError(f)
}

func (c *IncomingInvocation) Accept(ctx context.Context, buf []byte) error {
	if err := transmit(ctx, c.conn, c.tx, c.rx, buf); err != nil {
		return fmt.Errorf("failed to transmit accept: %w", err)
	}
	return nil
}

type Transmitter struct {
	conn    *nats.Conn
	subject string
}

func (tx *Transmitter) Transmit(ctx context.Context, path []uint32, buf []byte) error {
	subject := tx.subject
	for _, p := range path {
		subject = fmt.Sprintf("%s.%d", subject, p)
	}
	return transmit(ctx, tx.conn, subject, "", buf)
}

type Client struct {
	conn   *nats.Conn
	prefix string
}

func NewClient(conn *nats.Conn, prefix string) *Client {
	return &Client{conn, prefix}
}

func (c *Client) Serve(instance string, name string, f func(context.Context, []byte, wrpc.Transmitter, wrpc.IncomingInvocation) error) (stop func() error, err error) {
	sub, err := c.conn.Subscribe(invocationSubject(c.prefix, instance, name), func(m *nats.Msg) {
		if m.Reply == "" {
			log.Println("peer did not specify a reply subject")
			return
		}
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)
		if err := f(ctx, m.Data, &Transmitter{
			conn:    c.conn,
			subject: resultSubject(m.Reply),
		}, &IncomingInvocation{
			invocation: invocation{
				conn: c.conn,
				rx:   nats.NewInbox(),
				tx:   m.Reply,
			},
		}); err != nil {
			log.Printf("failed to handle `handle`: %s", err)
			b, err := wrpc.AppendString([]byte{}, fmt.Sprintf("%s", err))
			if err != nil {
				log.Printf("failed to encode `handle` handling error: %s", err)
				// Encoding the error failed, let's try encoding the encoding error
				b, err = wrpc.AppendString([]byte{}, fmt.Sprintf("failed to encode error: %s", err))
				if err != nil {
					log.Printf("failed to encode `handle` handling error encoding error: %s", err)
					// Well, we're out of luck at this point, let's just send an empty string
					b = []byte{0}
				}
			}
			if err = transmit(context.Background(), c.conn, fmt.Sprintf("%s.error", m.Reply), "", b); err != nil {
				log.Printf("failed to send error to client: %s", err)
			}
		}
	})
	if err != nil {
		return nil, fmt.Errorf("failed to serve `%s` for instance `%s`: %w", name, instance, err)
	}
	return sub.Unsubscribe, nil
}
