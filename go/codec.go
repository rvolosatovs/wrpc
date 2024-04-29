package wrpc

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"math"

	"github.com/tetratelabs/wabin/leb128"
)

type Ready interface {
	Ready() bool
}

type Receiver[T any] interface {
	Receive() (T, error)
}

type ReadyReceiver[T any] interface {
	Receiver[T]
	Ready
}

type ReadyReader interface {
	io.Reader
	Ready
}

type ReadyByteReader interface {
	ByteReader
	Ready
}

type byteReader struct {
	*bytes.Reader
}

func (r *byteReader) Read(b []byte) (int, error) {
	return r.Reader.Read(b)
}

func (r *byteReader) ReadByte() (byte, error) {
	return r.Reader.ReadByte()
}

func (*byteReader) Ready() bool {
	return true
}

type ready[T any] struct {
	v T
}

func (r *ready[T]) Receive() (T, error) {
	return r.v, nil
}

func (*ready[T]) Ready() bool {
	return true
}

type decodeReader[T any] struct {
	ByteReader
	decode func(ByteReader) (T, error)
}

func (r *decodeReader[T]) Receive() (T, error) {
	return r.decode(r.ByteReader)
}

func (*decodeReader[T]) Ready() bool {
	return false
}

func EncodeInt16(v int16) []byte {
	return leb128.EncodeInt32(int32(v))
}

func EncodeInt32(v int32) []byte {
	return leb128.EncodeInt32(v)
}

func EncodeInt64(v int64) []byte {
	return leb128.EncodeInt64(v)
}

func EncodeInt(v int) []byte {
	return leb128.EncodeInt64(int64(v))
}

func PutUint16(buf []byte, x uint16) {
	binary.PutUvarint(buf, uint64(x))
}

func AppendUint16(buf []byte, x uint16) []byte {
	return binary.AppendUvarint(buf, uint64(x))
}

func PutUint32(buf []byte, x uint32) {
	binary.PutUvarint(buf, uint64(x))
}

func AppendUint32(buf []byte, x uint32) []byte {
	return binary.AppendUvarint(buf, uint64(x))
}

func PutUint64(buf []byte, x uint64) {
	binary.PutUvarint(buf, x)
}

func AppendUint64(buf []byte, x uint64) []byte {
	return binary.AppendUvarint(buf, x)
}

func AppendFloat32(buf []byte, x float32) []byte {
	return binary.LittleEndian.AppendUint32(buf, math.Float32bits(x))
}

func PutFloat32(buf []byte, x float32) {
	binary.LittleEndian.PutUint32(buf, math.Float32bits(x))
}

func AppendFloat64(buf []byte, x float64) []byte {
	return binary.LittleEndian.AppendUint64(buf, math.Float64bits(x))
}

func PutFloat64(buf []byte, x float64) {
	binary.LittleEndian.PutUint64(buf, math.Float64bits(x))
}

func AppendString(buf []byte, s string) ([]byte, error) {
	n := len(s)
	if n > math.MaxUint32 {
		return nil, fmt.Errorf("response UTF-8 string byte length of %d overflows u32", n)
	}
	return append(append(buf, leb128.EncodeUint32(uint32(n))...), s...), nil
}

func WriteString(w io.Writer, s string) (int, error) {
	n := len(s)
	if n > math.MaxUint32 {
		return 0, fmt.Errorf("response UTF-8 string byte length of %d overflows u32", n)
	}
	n, err := w.Write(leb128.EncodeUint32(uint32(n)))
	if err != nil {
		return n, fmt.Errorf("failed to write string length of %d: %w", n, err)
	}
	sn, err := w.Write([]byte(s))
	if sn > math.MaxInt-n {
		return math.MaxInt, errors.New("encoded string length overflows int")
	}
	n += sn
	if err != nil {
		return n, fmt.Errorf("failed to write string: %w", err)
	}
	return n, nil
}

// ReadUint64 reads an encoded uint64 from r and returns it.
// The error is [io.EOF] only if no bytes were read.
// If an [io.EOF] happens after reading some but not all the bytes,
// ReadUvarint returns [io.ErrUnexpectedEOF].
func ReadUint64(r io.ByteReader) (uint64, error) {
	return binary.ReadUvarint(r)
}

type ByteReader interface {
	io.ByteReader
	io.Reader
}

func ReadString(r ByteReader) (string, error) {
	n, err := ReadUint32(r)
	if err != nil {
		return "", fmt.Errorf("failed to read string length: %w", err)
	}
	b := make([]byte, int(n))
	_, err = r.Read(b)
	if err != nil {
		return "", fmt.Errorf("failed to read string: %w", err)
	}
	return string(b), nil
}

// ReadOptionStatus reads a single byte from `r` and returns:
// - `true` for `option::some`
// - `false` for `option::none`
func ReadOptionStatus(r io.ByteReader) (bool, error) {
	status, err := r.ReadByte()
	if err != nil {
		return false, fmt.Errorf("failed to read `option` status byte: %w", err)
	}
	switch status {
	case 0:
		return false, nil
	case 1:
		return true, nil
	default:
		return false, fmt.Errorf("invalid `option` status byte %d", status)
	}
}

// ReadOption reads an option from `r`
func ReadOption[T any](r ByteReader, f func(ByteReader) (T, error)) (*T, error) {
	ok, err := ReadOptionStatus(r)
	if err != nil {
		return nil, err
	}
	if !ok {
		return nil, nil
	}
	v, err := f(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read `option::some` value: %w", err)
	}
	return &v, nil
}

// ReadResultStatus reads a single byte from `r` and returns:
// - `true` for `result::ok`
// - `false` for `result::err`
func ReadResultStatus(r io.ByteReader) (bool, error) {
	status, err := r.ReadByte()
	if err != nil {
		return false, fmt.Errorf("failed to read `result` status byte: %w", err)
	}
	switch status {
	case 0:
		return true, nil
	case 1:
		return false, nil
	default:
		return false, fmt.Errorf("invalid `result` status byte %d", status)
	}
}

// ReadResult reads a single byte from `r`
func ReadResult[T, U any](r ByteReader, fOk func(ByteReader) (T, error), fErr func(ByteReader) (U, error)) (*T, *U, error) {
	ok, err := ReadResultStatus(r)
	if err != nil {
		return nil, nil, err
	}
	if !ok {
		v, err := fErr(r)
		if err != nil {
			return nil, nil, fmt.Errorf("failed to read `result::err` value: %w", err)
		}
		return nil, &v, nil
	}
	v, err := fOk(r)
	if err != nil {
		return nil, nil, fmt.Errorf("failed to read `result::ok` value: %w", err)
	}
	return &v, nil, nil
}

// ReadByteList reads a []byte from `r` and returns it
func ReadByteList(r ByteReader) ([]byte, error) {
	n, err := ReadUint32(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read list length: %w", err)
	}
	vs := make([]byte, n)
	rn, err := r.Read(vs)
	if err != nil {
		return nil, fmt.Errorf("failed to read list bytes: %w", err)
	}
	if rn > int(n) {
		return nil, fmt.Errorf("invalid amount of list bytes read, expected %d, got %d", n, rn)
	}
	return vs, nil
}

// ReadList reads a list from `r` and returns it
func ReadList[T any](r ByteReader, f func(ByteReader) (T, error)) ([]T, error) {
	n, err := ReadUint32(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read list length: %w", err)
	}
	vs := make([]T, n)
	for i := range vs {
		v, err := f(r)
		if err != nil {
			return nil, fmt.Errorf("failed to read list element %d: %w", i, err)
		}
		vs[i] = v
	}
	return vs, nil
}

type Tuple2[T0, T1 any] struct {
	V0 T0
	V1 T1
}

func ReadTuple2[T0, T1 any](r ByteReader, f0 func(ByteReader) (T0, error), f1 func(ByteReader) (T1, error)) (*Tuple2[T0, T1], error,
) {
	v0, err := f0(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read tuple element 0: %w", err)
	}
	v1, err := f1(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read tuple element 1: %w", err)
	}
	return &Tuple2[T0, T1]{v0, v1}, nil
}

type Tuple3[T0, T1, T2 any] struct {
	V0 T0
	V1 T1
	V2 T2
}

func ReadTuple3[T0, T1, T2 any](r ByteReader, f0 func(ByteReader) (T0, error), f1 func(ByteReader) (T1, error), f2 func(ByteReader) (T2, error)) (*Tuple3[T0, T1, T2], error,
) {
	v0, err := f0(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read tuple element 0: %w", err)
	}
	v1, err := f1(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read tuple element 1: %w", err)
	}
	v2, err := f2(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read tuple element 2: %w", err)
	}
	return &Tuple3[T0, T1, T2]{v0, v1, v2}, nil
}

// ReadStreamStatus reads a single byte from `r` and returns:
// - `true` if stream is "ready"
// - `false` if stream is "pending"
func ReadStreamStatus(r io.ByteReader) (bool, error) {
	status, err := r.ReadByte()
	if err != nil {
		return false, fmt.Errorf("failed to read `stream` status byte: %w", err)
	}
	switch status {
	case 0:
		return true, nil
	case 1:
		return false, nil
	default:
		return false, fmt.Errorf("invalid `stream` status byte %d", status)
	}
}

// ReadByteStream reads a stream of bytes from `r` and `ch`
func ReadByteStream(ctx context.Context, r ByteReader, ch <-chan []byte) (ReadyReader, error) {
	ok, err := ReadStreamStatus(r)
	if err != nil {
		return nil, err
	}
	if !ok {
		return &decodeReader[[]byte]{&ChanReader{ctx, ch, nil}, func(r ByteReader) ([]byte, error) {
			n, err := ReadUint32(r)
			if err != nil {
				return nil, fmt.Errorf("failed to read pending stream chunk length: %w", err)
			}
			if n == 0 {
				return nil, io.EOF
			}
			b := make([]byte, n)
			rn, err := r.Read(b)
			if err != nil {
				return nil, fmt.Errorf("failed to read pending stream chunk bytes: %w", err)
			}
			if rn > int(n) {
				return nil, fmt.Errorf("invalid amount of pending stream chunk bytes read, expected %d, got %d", n, rn)
			}
			return b, nil
		}}, nil
	}
	buf, err := ReadByteList(r)
	if err != nil {
		return nil, fmt.Errorf("failed to read bytes: %w", err)
	}
	return &byteReader{bytes.NewReader(buf)}, nil
}

// ReadStream reads a stream from `r` and `ch`
func ReadStream[T any](ctx context.Context, r ByteReader, ch <-chan []byte, f func(ByteReader) (T, error)) (ReadyReceiver[[]T], error) {
	ok, err := ReadStreamStatus(r)
	if err != nil {
		return nil, err
	}
	if !ok {
		return &decodeReader[[]T]{&ChanReader{ctx, ch, nil}, func(r ByteReader) ([]T, error) {
			n, err := ReadUint32(r)
			if err != nil {
				return nil, fmt.Errorf("failed to read pending stream chunk length: %w", err)
			}
			if n == 0 {
				return nil, io.EOF
			}
			vs := make([]T, n)
			for i := range vs {
				v, err := f(r)
				if err != nil {
					return nil, fmt.Errorf("failed to read pending stream chunk element %d: %w", i, err)
				}
				vs[i] = v
			}
			return vs, nil
		}}, nil
	}
	vs, err := ReadList(r, f)
	if err != nil {
		return nil, fmt.Errorf("failed to read ready stream: %w", err)
	}
	return &ready[[]T]{vs}, nil
}

// ReadFutureStatus reads a single byte from `r` and returns:
// - `true` if future is "ready"
// - `false` if future is "pending"
func ReadFutureStatus(r io.ByteReader) (bool, error) {
	status, err := r.ReadByte()
	if err != nil {
		return false, fmt.Errorf("failed to read `future` status byte: %w", err)
	}
	switch status {
	case 0:
		return true, nil
	case 1:
		return false, nil
	default:
		return false, fmt.Errorf("invalid `future` status byte %d", status)
	}
}

// ReadFuture reads a future from `r` and `ch`
func ReadFuture[T any](ctx context.Context, r ByteReader, ch <-chan []byte, f func(ByteReader) (T, error)) (ReadyReceiver[T], error) {
	ok, err := ReadFutureStatus(r)
	if err != nil {
		return nil, err
	}
	if !ok {
		return &decodeReader[T]{&ChanReader{ctx, ch, nil}, f}, nil
	}
	v, err := f(r)
	if err != nil {
		return nil, err
	}
	return &ready[T]{v}, nil
}

// NOTE: Below is adapted from https://cs.opensource.google/go/go/+/refs/tags/go1.22.2:src/encoding/binary/varint.go;l=128-153

// maxVarintLenN is the maximum length of a varint-encoded N-bit integer.
const (
	maxVarintLen16 = 3
	maxVarintLen32 = 5
)

var errOverflow16 = errors.New("wrpc: varint overflows a 16-bit integer")

// ReadUint16 reads an encoded uint16 from r and returns it.
// The error is [io.EOF] only if no bytes were read.
// If an [io.EOF] happens after reading some but not all the bytes,
// ReadUvarint returns [io.ErrUnexpectedEOF].
func ReadUint16(r io.ByteReader) (uint16, error) {
	var x uint16
	var s uint
	for i := 0; i < maxVarintLen16; i++ {
		b, err := r.ReadByte()
		if err != nil {
			if i > 0 && err == io.EOF {
				err = io.ErrUnexpectedEOF
			}
			return x, err
		}
		if b < 0x80 {
			if i == maxVarintLen16-1 && b > 1 {
				return x, errOverflow16
			}
			return x | uint16(b)<<s, nil
		}
		x |= uint16(b&0x7f) << s
		s += 7
	}
	return x, errOverflow16
}

var errOverflow32 = errors.New("wrpc: varint overflows a 32-bit integer")

// ReadUint32 reads an encoded uint32 from r and returns it.
// The error is [io.EOF] only if no bytes were read.
// If an [io.EOF] happens after reading some but not all the bytes,
// ReadUvarint returns [io.ErrUnexpectedEOF].
func ReadUint32(r io.ByteReader) (uint32, error) {
	var x uint32
	var s uint
	for i := 0; i < maxVarintLen32; i++ {
		b, err := r.ReadByte()
		if err != nil {
			if i > 0 && err == io.EOF {
				err = io.ErrUnexpectedEOF
			}
			return x, err
		}
		if b < 0x80 {
			if i == maxVarintLen32-1 && b > 1 {
				return x, errOverflow32
			}
			return x | uint32(b)<<s, nil
		}
		x |= uint32(b&0x7f) << s
		s += 7
	}
	return x, errOverflow32
}
