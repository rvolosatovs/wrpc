package types

import (
	"context"
	"fmt"
	"io"
	"log"

	wrpc "github.com/wrpc/wrpc/go"
)

type Reader struct {
	ctx    context.Context
	ch     <-chan []byte
	buffer []byte
}

func (r *Reader) Read(p []byte) (int, error) {
	if len(r.buffer) > 0 {
		n := copy(p, r.buffer)
		r.buffer = r.buffer[n:]
		return n, nil
	}
	select {
	case buf, ok := <-r.ch:
		if !ok {
			return 0, io.EOF
		}
		n := copy(p, buf)
		r.buffer = buf[n:]
		return n, nil
	case <-r.ctx.Done():
		return 0, r.ctx.Err()
	}
}

func (r *Reader) ReadByte() (byte, error) {
	if len(r.buffer) > 0 {
		b := r.buffer[0]
		r.buffer = r.buffer[1:]
		return b, nil
	}
	for {
		select {
		case buf, ok := <-r.ch:
			if !ok {
				return 0, io.EOF
			}
			if len(buf) < 1 {
				continue
			}
			r.buffer = buf[1:]
			return buf[0], nil
		case <-r.ctx.Done():
			return 0, r.ctx.Err()
		}
	}
}

type Receiver[T any] interface {
	Receive() (T, error)
}

type ReadyReceiver[T any] struct {
	v T
}

func (r *ReadyReceiver[T]) Receive() (T, error) {
	return r.v, nil
}

type PendingReceiver[T any] struct {
	*Reader
	decode func(wrpc.ByteReader) (T, error)
}

func (r *PendingReceiver[T]) Receive() (T, error) {
	return r.decode(r.Reader)
}

type DiscriminantMethod uint32

const (
	DiscriminantMethod_Get DiscriminantMethod = iota
	DiscriminantMethod_Head
	DiscriminantMethod_Post
	DiscriminantMethod_Put
	DiscriminantMethod_Delete
	DiscriminantMethod_Connect
	DiscriminantMethod_Options
	DiscriminantMethod_Trace
	DiscriminantMethod_Patch
	DiscriminantMethod_Other
)

type VariantMethod struct {
	Payload      any
	Discriminant DiscriminantMethod
}

func (v *VariantMethod) PayloadOther() (string, bool) {
	if v.Discriminant != DiscriminantMethod_Other {
		return "", false
	}
	p, ok := v.Payload.(string)
	return p, ok
}

func ReadMethod(r wrpc.ByteReader) (*VariantMethod, error) {
	disc, err := wrpc.ReadUint32(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read `method` discriminant: %w", err)
	}
	switch DiscriminantMethod(disc) {
	case DiscriminantMethod_Get:
		return &VariantMethod{nil, DiscriminantMethod_Get}, nil
	case DiscriminantMethod_Head:
		return &VariantMethod{nil, DiscriminantMethod_Head}, nil
	case DiscriminantMethod_Post:
		return &VariantMethod{nil, DiscriminantMethod_Post}, nil
	case DiscriminantMethod_Delete:
		return &VariantMethod{nil, DiscriminantMethod_Delete}, nil
	case DiscriminantMethod_Connect:
		return &VariantMethod{nil, DiscriminantMethod_Connect}, nil
	case DiscriminantMethod_Options:
		return &VariantMethod{nil, DiscriminantMethod_Options}, nil
	case DiscriminantMethod_Trace:
		return &VariantMethod{nil, DiscriminantMethod_Trace}, nil
	case DiscriminantMethod_Patch:
		return &VariantMethod{nil, DiscriminantMethod_Patch}, nil
	case DiscriminantMethod_Other:
		payload, err := wrpc.ReadString(r)
		if err != nil {
			return nil, fmt.Errorf("failed to read `method::other` value: %w", err)
		}
		return &VariantMethod{payload, DiscriminantMethod_Other}, nil
	default:
		return nil, fmt.Errorf("unknown `method` discriminant value %d", disc)
	}
}

type DiscriminantScheme uint32

const (
	DiscriminantScheme_Http DiscriminantScheme = iota
	DiscriminantScheme_Https
	DiscriminantScheme_Other
)

type VariantScheme struct {
	Payload      any
	Discriminant DiscriminantScheme
}

func (v *VariantScheme) PayloadOther() (string, bool) {
	if v.Discriminant != DiscriminantScheme_Other {
		return "", false
	}
	p, ok := v.Payload.(string)
	return p, ok
}

func ReadScheme(r wrpc.ByteReader) (*VariantScheme, error) {
	disc, err := wrpc.ReadUint32(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read `scheme` discriminant: %w", err)
	}
	switch DiscriminantScheme(disc) {
	case DiscriminantScheme_Http:
		return &VariantScheme{nil, DiscriminantScheme_Http}, nil
	case DiscriminantScheme_Https:
		return &VariantScheme{nil, DiscriminantScheme_Https}, nil
	case DiscriminantScheme_Other:
		payload, err := wrpc.ReadString(r)
		if err != nil {
			return nil, fmt.Errorf("failed to read `scheme::other` value: %w", err)
		}
		return &VariantScheme{payload, DiscriminantScheme_Other}, nil
	default:
		return nil, fmt.Errorf("unknown `scheme` discriminant value %d", disc)
	}
}

type (
	SubscriptionRequest struct {
		payload         <-chan []byte
		payloadBody     <-chan []byte
		payloadTrailers <-chan []byte

		stop         func() error
		stopBody     func() error
		stopTrailers func() error

		errorsBody     <-chan error
		errorsTrailers <-chan error

		buffer []byte
	}
	RecordRequest struct {
		Body          wrpc.ReadyReader
		Trailers      wrpc.ReadyReceiver[[]*wrpc.Tuple2[string, [][]byte]]
		Method        VariantMethod
		PathWithQuery *string
		Scheme        *VariantScheme
		Authority     *string
		Headers       []*wrpc.Tuple2[string, [][]byte]
	}

	SubscriptionResponse struct {
		payload         <-chan []byte
		payloadBody     <-chan []byte
		payloadTrailers <-chan []byte

		stop         func() error
		stopBody     func() error
		stopTrailers func() error

		errorsBody     <-chan error
		errorsTrailers <-chan error

		buffer []byte
	}
	RecordResponse struct {
		Body     wrpc.ReadyReader
		Trailers wrpc.ReadyReceiver[[]*wrpc.Tuple2[string, [][]byte]]
		Status   uint16
		Headers  []*wrpc.Tuple2[string, [][]byte]
	}
)

func SubscribeRequest(sub wrpc.Subscriber, buffer []byte) (*SubscriptionRequest, error) {
	// TODO: Only subscribe for data not contained in `buffer`

	payload := make(chan []byte)
	stop, err := sub.Subscribe(func(ctx context.Context, buf []byte) {
		payload <- buf
	})
	if err != nil {
		return nil, err
	}

	payloadBody := make(chan []byte)
	errorsBody := make(chan error)
	stopBody, err := sub.SubscribePath([]*uint32{wrpc.Uint32(0), wrpc.Uint32(0)}, errorsBody, func(ctx context.Context, path []uint32, buf []byte) {
		payloadBody <- buf
	})
	if err != nil {
		defer func() {
			if err := stop(); err != nil {
				log.Println("failed to stop payload subsciption")
			}
		}()
		return nil, err
	}

	payloadTrailers := make(chan []byte)
	errorsTrailers := make(chan error)
	stopTrailers, err := sub.SubscribePath([]*uint32{wrpc.Uint32(0), wrpc.Uint32(1)}, errorsTrailers, func(ctx context.Context, path []uint32, buf []byte) {
		payloadTrailers <- buf
	})
	if err != nil {
		defer func() {
			if err := stop(); err != nil {
				log.Println("failed to stop payload subsciption")
			}
		}()
		defer func() {
			if err := stopBody(); err != nil {
				log.Println("failed to stop `body` subsciption")
			}
		}()
		return nil, err
	}

	return &SubscriptionRequest{
		payload,
		payloadBody,
		payloadTrailers,
		stop,
		stopBody,
		stopTrailers,
		errorsBody,
		errorsTrailers,
		buffer,
	}, nil
}

func (s *SubscriptionRequest) Receive(ctx context.Context) (*RecordRequest, error) {
	r := &Reader{
		ctx, s.payload, s.buffer,
	}
	body, err := wrpc.ReadByteStream(ctx, r, s.payloadBody)
	if err != nil {
		return nil, fmt.Errorf("failed to receive `body`: %w", err)
	}
	trailers, err := wrpc.ReadFuture(ctx, r, s.payloadTrailers, func(r wrpc.ByteReader) ([]*wrpc.Tuple2[string, [][]byte], error) {
		return wrpc.ReadList(r, func(r wrpc.ByteReader) (*wrpc.Tuple2[string, [][]byte], error) {
			return wrpc.ReadTuple2(r, wrpc.ReadString, func(r wrpc.ByteReader) ([][]byte, error) {
				return wrpc.ReadList(r, wrpc.ReadByteList)
			})
		})
	})
	if err != nil {
		return nil, fmt.Errorf("failed to receive `trailers`: %w", err)
	}
	method, err := ReadMethod(r)
	if err != nil {
		return nil, fmt.Errorf("failed to receive `method`: %w", err)
	}
	pathWithQuery, err := wrpc.ReadOption(r, wrpc.ReadString)
	if err != nil {
		return nil, fmt.Errorf("failed to receive `path-with-query`: %w", err)
	}
	scheme, err := ReadScheme(r)
	if err != nil {
		return nil, fmt.Errorf("failed to receive `scheme`: %w", err)
	}
	authority, err := wrpc.ReadOption(r, wrpc.ReadString)
	if err != nil {
		return nil, fmt.Errorf("failed to receive `authority`: %w", err)
	}
	headers, err := wrpc.ReadList(r, func(r wrpc.ByteReader) (*wrpc.Tuple2[string, [][]byte], error) {
		return wrpc.ReadTuple2(r, wrpc.ReadString, func(r wrpc.ByteReader) ([][]byte, error) {
			return wrpc.ReadList(r, wrpc.ReadByteList)
		})
	})
	if err != nil {
		return nil, fmt.Errorf("failed to receive `headers`: %w", err)
	}
	return &RecordRequest{
		Body: body, Trailers: trailers, Method: *method, PathWithQuery: pathWithQuery, Scheme: scheme, Authority: authority, Headers: headers,
	}, nil
}
