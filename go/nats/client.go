package wrpcnats

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"sync"
	"sync/atomic"

	wrpc "gowrpc.lol/go"
	"github.com/nats-io/nats.go"
)

// Client is a thin wrapper around *nats.Conn, which is able to serve and invoke wRPC functions
type Client struct {
	conn   *nats.Conn
	prefix string
	group  string
}

// ClientOpt is option client configuration option passed to NewClient
type ClientOpt func(*Client)

// WithPrefix sets a prefix for this Client
func WithPrefix(prefix string) ClientOpt {
	return func(c *Client) {
		c.prefix = prefix
	}
}

// WithGroup sets a queue group for this Client
func WithGroup(group string) ClientOpt {
	return func(c *Client) {
		c.group = group
	}
}

func NewClient(conn *nats.Conn, opts ...ClientOpt) *Client {
	c := &Client{conn: conn}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

type headerKey struct{}

func HeaderFromContext(ctx context.Context) (nats.Header, bool) {
	v, ok := ctx.Value(headerKey{}).(nats.Header)
	return v, ok
}

func ContextWithHeader(ctx context.Context, header nats.Header) context.Context {
	return context.WithValue(ctx, headerKey{}, header)
}

func paramSubject(prefix string) string {
	return fmt.Sprintf("%s.params", prefix)
}

func resultSubject(prefix string) string {
	return fmt.Sprintf("%s.results", prefix)
}

func indexPath(prefix string, path ...uint32) string {
	s := prefix
	for _, p := range path {
		if s != "" {
			s = fmt.Sprintf("%s.%d", s, p)
		} else {
			s = fmt.Sprintf("%d", p)
		}
	}
	return s
}

func subscribePath(prefix string, path wrpc.SubscribePath) string {
	s := prefix
	for _, p := range path {
		if s != "" {
			s = fmt.Sprintf("%s.", s)
		}
		if p == nil {
			s = fmt.Sprintf("%s*", s)
		} else {
			s = fmt.Sprintf("%s%d", s, *p)
		}
	}
	return s
}

func invocationSubject(prefix string, instance string, name string) string {
	subject := fmt.Sprintf("wrpc.0.0.1.%s.%s", instance, name)
	if prefix != "" {
		return fmt.Sprintf("%s.%s", prefix, subject)
	}
	return subject
}

func subscribe(conn *nats.Conn, prefix string, f func(context.Context, []byte), path ...uint32) (*nats.Subscription, error) {
	subject := prefix
	for _, p := range path {
		subject = fmt.Sprintf("%s.%d", subject, p)
	}
	return conn.Subscribe(subject, func(m *nats.Msg) {
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)
		f(ctx, m.Data)
	})
}

type paramWriter struct {
	nc   *nats.Conn
	init func() (*initState, error)
	path string
}

type initState struct {
	tx  string
	buf []byte
}

func (w *paramWriter) Write(p []byte) (int, error) {
	init, err := w.init()
	if err != nil {
		return 0, fmt.Errorf("failed to perform handshake")
	}
	tx := init.tx
	if w.path != "" {
		tx = fmt.Sprintf("%s.%s", tx, w.path)
	}

	buf := p
	if w.path == "" && len(init.buf) > 0 {
		buf = append(init.buf, p...)
	}
	maxPayload := w.nc.MaxPayload()
	pn := len(p)
	n := 0
	for len(buf) > 0 {
		maxPayload = min(maxPayload, int64(len(buf)))
		p, buf = buf[:maxPayload], buf[maxPayload:]
		if err := w.nc.Publish(tx, p); err != nil {
			if w.path == "" {
				bn := len(init.buf)
				if n < bn {
					init.buf = init.buf[n:]
					n = 0
				} else {
					n -= bn
					init.buf = nil
				}
			}
			return n, fmt.Errorf("failed to send payload chunk: %w", err)
		}
		n += len(p)
	}
	if w.path == "" {
		init.buf = nil
	}
	return pn, nil
}

func (w *paramWriter) WriteByte(b byte) error {
	_, err := w.Write([]byte{b})
	if err != nil {
		return err
	}
	return nil
}

func (w *paramWriter) Index(path ...uint32) (wrpc.IndexWriter, error) {
	return &paramWriter{
		nc:   w.nc,
		init: w.init,
		path: indexPath(w.path, path...),
	}, nil
}

func (w *paramWriter) Close() error {
	init, err := w.init()
	if err != nil {
		return fmt.Errorf("failed to perform handshake")
	}
	tx := init.tx
	if w.path != "" {
		tx = fmt.Sprintf("%s.%s", tx, w.path)
	}
	if err := w.nc.Publish(tx, nil); err != nil {
		return fmt.Errorf("failed to send empty message to shut down stream")
	}
	return nil
}

type resultWriter struct {
	nc *nats.Conn
	tx string
}

func (w *resultWriter) Write(p []byte) (int, error) {
	n := len(p)
	maxPayload := w.nc.MaxPayload()
	maxPayload = min(maxPayload, int64(n))
	var buf []byte
	p, buf = p[:maxPayload], p[maxPayload:]
	if err := w.nc.Publish(w.tx, p); err != nil {
		return 0, fmt.Errorf("failed to send initial payload chunk: %w", err)
	}
	for len(buf) > 0 {
		maxPayload = min(maxPayload, int64(len(buf)))
		p, buf = buf[:maxPayload], buf[maxPayload:]
		if err := w.nc.Publish(w.tx, p); err != nil {
			return 0, fmt.Errorf("failed to send payload chunk: %w", err)
		}
	}
	return n, nil
}

func (w *resultWriter) WriteByte(b byte) error {
	if err := w.nc.Publish(w.tx, []byte{b}); err != nil {
		return fmt.Errorf("failed to send byte: %w", err)
	}
	return nil
}

func (w *resultWriter) Close() error {
	if err := w.nc.Publish(w.tx, nil); err != nil {
		return fmt.Errorf("failed to send shutdown message: %w", err)
	}
	return nil
}

func (w *resultWriter) Index(path ...uint32) (wrpc.IndexWriter, error) {
	return &resultWriter{nc: w.nc, tx: indexPath(w.tx, path...)}, nil
}

type streamReader struct {
	ctx context.Context
	sub *nats.Subscription
	// poor man's [`std::sync::Arc`](https://doc.rust-lang.org/std/sync/struct.Arc.html)
	nestMu  *sync.Mutex
	nestRef *atomic.Int64
	nest    map[string]*nats.Subscription
	path    string
	buf     []byte
}

func (r *streamReader) Read(p []byte) (int, error) {
	if len(r.buf) > 0 {
		n := copy(p, r.buf)
		slog.Debug("copied bytes from buffer", "requested", len(p), "buffered", len(r.buf), "copied", n)
		r.buf = r.buf[n:]
		return n, nil
	}
	slog.Debug("receiving next byte chunk")
	msg, err := r.sub.NextMsgWithContext(r.ctx)
	if err != nil {
		return 0, err
	}
	n := copy(p, msg.Data)
	r.buf = msg.Data[n:]
	return n, nil
}

func (r *streamReader) ReadByte() (byte, error) {
	if len(r.buf) > 0 {
		b := r.buf[0]
		slog.Debug("copied byte from buffer", "buffered", len(r.buf))
		r.buf = r.buf[1:]
		return b, nil
	}
	for {
		slog.Debug("receiving next byte chunk")
		msg, err := r.sub.NextMsgWithContext(r.ctx)
		if err != nil {
			return 0, err
		}
		if len(msg.Data) == 0 {
			continue
		}
		r.buf = msg.Data[1:]
		return msg.Data[0], nil
	}
}

func (r *streamReader) Close() (err error) {
	var errs []error
	if err := r.sub.Unsubscribe(); err != nil {
		errs = append(errs, fmt.Errorf("failed to unsubscribe: %w", err))
	}
	refs := r.nestRef.Add(-1)
	if refs == 0 {
		// since this is the only reference to `nest`, no need to lock the mutex
		for path, sub := range r.nest {
			if err := sub.Unsubscribe(); err != nil {
				errs = append(errs, fmt.Errorf("failed to unsubscribe from nested path `%s`: %w", path, err))
			}
		}
	}
	if len(errs) > 0 {
		return fmt.Errorf("%v", errs)
	}
	return nil
}

func (r *streamReader) Index(path ...uint32) (wrpc.IndexReader, error) {
	r.nestRef.Add(1)
	s := indexPath(r.path, path...)
	r.nestMu.Lock()
	defer r.nestMu.Unlock()
	sub, ok := r.nest[s]
	if !ok {
		return nil, errors.New("unknown subscription")
	}
	delete(r.nest, s)
	return &streamReader{
		ctx:     r.ctx,
		sub:     sub,
		nestMu:  r.nestMu,
		nestRef: r.nestRef,
		nest:    r.nest,
		path:    s,
	}, nil
}

func (c *Client) Invoke(ctx context.Context, instance string, name string, buf []byte, paths ...wrpc.SubscribePath) (wrpc.IndexWriteCloser, wrpc.IndexReadCloser, error) {
	rx := nats.NewInbox()

	resultRx := resultSubject(rx)
	slog.Debug("subscribing on result subject", "subject", resultRx)
	resultSub, err := c.conn.SubscribeSync(resultRx)
	if err != nil {
		return nil, nil, fmt.Errorf("failed to subscribe on result subject `%s`: %w", resultRx, err)
	}

	nest := make(map[string]*nats.Subscription, len(paths))
	for _, path := range paths {
		s := subscribePath(resultRx, path)
		slog.Debug("subscribing on nested result subject", "subject", s)
		sub, err := c.conn.SubscribeSync(s)
		if err != nil {
			return nil, nil, fmt.Errorf("failed to subscribe on nested result subject `%s`: %w", s, err)
		}
		nest[subscribePath("", path)] = sub
	}
	nestRef := &atomic.Int64{}
	nestRef.Add(1)
	return &paramWriter{
			nc: c.conn,
			init: sync.OnceValues(func() (init *initState, err error) {
				header, hasHeader := HeaderFromContext(ctx)

				m := nats.NewMsg(invocationSubject(c.prefix, instance, name))
				m.Reply = rx
				if hasHeader {
					m.Header = header
				}

				maxPayload := c.conn.MaxPayload()
				mSize := int64(m.Size())
				if mSize > maxPayload {
					return nil, fmt.Errorf("message size %d is larger than maximum allowed payload size %d", mSize, maxPayload)
				}
				maxPayload -= mSize
				maxPayload = min(maxPayload, int64(len(buf)))
				m.Data, buf = buf[:maxPayload], buf[maxPayload:]

				sub, err := c.conn.SubscribeSync(rx)
				if err != nil {
					return nil, fmt.Errorf("failed to subscribe on Rx subject: %w", err)
				}
				defer func() {
					if sErr := sub.Unsubscribe(); sErr != nil {
						if err == nil {
							err = fmt.Errorf("failed to unsubscribe from handshake subject: %w", sErr)
						} else {
							slog.Error("failed to unsubscribe from handshake subject", "err", sErr)
						}
					}
				}()

				slog.DebugContext(ctx, "publishing handshake", "rx", m.Reply)
				if err := c.conn.PublishMsg(m); err != nil {
					return nil, fmt.Errorf("failed to send initial payload chunk: %w", err)
				}
				m, err = sub.NextMsgWithContext(ctx)
				if err != nil {
					return nil, fmt.Errorf("failed to receive handshake: %w", err)
				}
				if m.Reply == "" {
					return nil, errors.New("peer did not specify a reply subject")
				}
				return &initState{
					buf: buf,
					tx:  paramSubject(m.Reply),
				}, nil
			}),
		}, &streamReader{
			ctx:     ctx,
			sub:     resultSub,
			nestMu:  &sync.Mutex{},
			nestRef: nestRef,
			nest:    nest,
		}, nil
}

func (c *Client) handleMessage(instance string, name string, f func(context.Context, wrpc.IndexWriteCloser, wrpc.IndexReadCloser), paths ...wrpc.SubscribePath) func(m *nats.Msg) {
	return func(m *nats.Msg) {
		ctx := context.Background()
		ctx = ContextWithHeader(ctx, m.Header)

		slog.Debug("received invocation", "instance", instance, "name", name, "payload", m.Data, "reply", m.Reply)
		if m.Reply == "" {
			slog.Warn("peer did not specify a reply subject")
			return
		}

		rx := nats.NewInbox()

		paramRx := paramSubject(rx)
		slog.Debug("subscribing on parameter subject", "subject", paramRx)
		paramSub, err := c.conn.SubscribeSync(paramRx)
		if err != nil {
			slog.Warn("failed to subscribe on parameter subject", "subject", paramRx, "err", err)
			return
		}

		nest := make(map[string]*nats.Subscription, len(paths))
		for _, path := range paths {
			s := subscribePath(paramRx, path)
			slog.Debug("subscribing on nested parameter subject", "subject", s)
			sub, err := c.conn.SubscribeSync(s)
			if err != nil {
				slog.Warn("failed to subscribe on nested parameter subject", "subject", s, "err", err)
				return
			}
			nest[subscribePath("", path)] = sub
		}

		slog.DebugContext(ctx, "publishing handshake response", "subject", m.Reply, "reply", rx)
		accept := nats.NewMsg(m.Reply)
		accept.Reply = rx
		if err := c.conn.PublishMsg(accept); err != nil {
			slog.Error("failed to send handshake", "err", err)
			return
		}

		slog.Debug("calling server handler")
		nestRef := &atomic.Int64{}
		nestRef.Add(1)
		f(ctx, &resultWriter{
			nc: c.conn,
			tx: resultSubject(m.Reply),
		}, &streamReader{
			ctx:     ctx,
			sub:     paramSub,
			buf:     m.Data,
			nestMu:  &sync.Mutex{},
			nestRef: nestRef,
			nest:    nest,
		})
		slog.Debug("finished serving invocation")
	}
}

func (c *Client) Serve(instance string, name string, f func(context.Context, wrpc.IndexWriteCloser, wrpc.IndexReadCloser), paths ...wrpc.SubscribePath) (stop func() error, err error) {
	slog.Debug("serving", "instance", instance, "name", name, "group", c.group)

	subject := invocationSubject(c.prefix, instance, name)
	handle := c.handleMessage(instance, name, f, paths...)
	var sub *nats.Subscription
	if c.group != "" {
		sub, err = c.conn.QueueSubscribe(subject, c.group, handle)
	} else {
		sub, err = c.conn.Subscribe(subject, handle)
	}
	if err != nil {
		return nil, fmt.Errorf("failed to serve `%s` for instance `%s`: %w", name, instance, err)
	}
	return sub.Unsubscribe, nil
}
