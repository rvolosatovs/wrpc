package wrpc

import (
	"context"
	"io"
)

func Ptr[T any](v T) *T {
	return &v
}

func Int8(v int8) *int8 {
	return &v
}

func Int16(v int16) *int16 {
	return &v
}

func Int32(v int32) *int32 {
	return &v
}

func Int64(v int64) *int64 {
	return &v
}

func Int(v int) *int {
	return &v
}

func Uint8(v uint8) *uint8 {
	return &v
}

func Uint16(v uint16) *uint16 {
	return &v
}

func Uint32(v uint32) *uint32 {
	return &v
}

func Uint64(v uint64) *uint64 {
	return &v
}

func Uint(v uint) *uint {
	return &v
}

func Rune(v rune) *rune {
	return &v
}

func String(v string) *string {
	return &v
}

type ChanReader struct {
	ctx    context.Context
	ch     <-chan []byte
	buffer []byte
}

func (r *ChanReader) Read(p []byte) (int, error) {
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

func (r *ChanReader) ReadByte() (byte, error) {
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
