package outgoing_handler

import (
	"context"
	"errors"
	"fmt"
	"io"
	"math"
	"net/http"

	wrpc "github.com/wrpc/wrpc/go"
	"github.com/wrpc/wrpc/go/interface/http/types"
)

func ServeHandle(c wrpc.Client, f func(*types.RecordRequest) error) (func() error, error) {
	return c.Serve("wrpc:http/outgoing-handler@0.1.0", "handle", func(ctx context.Context, buf []byte, tx wrpc.Transmitter, inv wrpc.IncomingInvocation) error {
		sub, err := types.SubscribeRequest(inv, buf)
		if err != nil {
			return fmt.Errorf("failed to subscribe for `wrpc:http/types.Request`: %w", err)
		}
		if err := inv.Accept(ctx, nil); err != nil {
			return fmt.Errorf("failed to complete handshake: %w", err)
		}
		req, err := sub.Receive(ctx)
		if err != nil {
			return fmt.Errorf("failed to receive `wrpc:http/types.Request`: %w", err)
		}
		err = f(req)
		if err != nil {
			return fmt.Errorf("failed to handle `wrpc:http/outgoing-handler.handle`: %w", err)
		}

		scheme := ""
		switch req.Scheme.Discriminant {
		case types.DiscriminantScheme_Http:
			scheme = "http"
		case types.DiscriminantScheme_Https:
			scheme = "https"
		case types.DiscriminantScheme_Other:
			var ok bool
			scheme, ok = req.Scheme.PayloadOther()
			if !ok {
				return errors.New("scheme is not valid")
			}
		}
		authority := ""
		if req.Authority != nil {
			authority = *req.Authority
		}
		pathWithQuery := ""
		if req.PathWithQuery != nil {
			pathWithQuery = *req.PathWithQuery
		}

		switch req.Method.Discriminant {
		case types.DiscriminantMethod_Get:
			resp, err := http.Get(fmt.Sprintf("%s://%s/%s", scheme, authority, pathWithQuery))
			if err != nil {
				return fmt.Errorf("request failed: %w", err)
			}

			respBody, err := io.ReadAll(resp.Body)
			if err != nil {
				return fmt.Errorf("failed to read body: %w", err)
			}
			res := []byte{0, 1}
			res = wrpc.AppendUint32(res, uint32(len(respBody)))
			res = append(res, respBody...)
			res = append(res, 1, 0) // empty trailers
			res = wrpc.AppendUint16(res, uint16(resp.StatusCode))

			nh := len(resp.Header)
			if nh > math.MaxUint32 {
				return fmt.Errorf("header count %d overflows uint32", nh)
			}
			res = wrpc.AppendUint32(res, uint32(nh))
			for header, values := range resp.Header {
				nv := len(values)
				if nv > math.MaxUint32 {
					return fmt.Errorf("header value count %d overflows uint32", nv)
				}
				res, err = wrpc.AppendString(res, header)
				if err != nil {
					return fmt.Errorf("failed to encode header name: %w", err)
				}
				res = wrpc.AppendUint32(res, uint32(nv))
				for _, value := range values {
					res, err = wrpc.AppendString(res, value)
					if err != nil {
						return fmt.Errorf("failed to encode header value: %w", err)
					}
				}
			}
			if err := tx.Transmit(context.Background(), nil, res); err != nil {
				return fmt.Errorf("failed to transmit result: %w", err)
			}

			return nil
		default:
			return errors.New("only GET currently supported")
		}
	})
}
