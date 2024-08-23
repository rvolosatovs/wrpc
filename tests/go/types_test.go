//go:generate $WIT_BINDGEN_WRPC go --gofmt=false --world types --out-dir bindings/types --package gowrpc.lol/tests/go/bindings/types ../wit

package integration_test

import (
	"bytes"
	"testing"

	"github.com/stretchr/testify/assert"

	wrpc "gowrpc.lol/go"
	"gowrpc.lol/tests/go/bindings/types/wrpc_test/integration/get_types"
)

type indexReader struct {
	*bytes.Buffer
}

func (r *indexReader) Index(path ...uint32) (wrpc.IndexReader, error) {
	panic("not implemented")
}

func TestTypes_Flags(t *testing.T) {
	t.Run("WriteToIndex", func(t *testing.T) {
		check := assert.New(t)

		flags := get_types.FeatureFlags{
			ShowA: true,
			ShowC: true,
		}
		var b bytes.Buffer
		_, err := flags.WriteToIndex(&b)
		check.NoError(err)
		check.Equal([]byte{0b00000101}, b.Bytes())
	})

	t.Run("ReadFromIndex", func(t *testing.T) {
		check := assert.New(t)

		var flags get_types.FeatureFlags
		err := flags.ReadFromIndex(&indexReader{
			Buffer: bytes.NewBuffer([]byte{0b00000101}),
		})
		check.NoError(err)
		check.Equal(get_types.FeatureFlags{
			ShowA: true,
			ShowC: true,
		}, flags)

		t.Run("invalid bit set", func(t *testing.T) {
			check := assert.New(t)

			var flags get_types.FeatureFlags
			err := flags.ReadFromIndex(&indexReader{
				Buffer: bytes.NewBuffer([]byte{0b10000000}),
			})
			if check.Error(err) {
				check.Equal("bit not associated with any flag is set", err.Error())
			}
			check.Zero(flags)
		})
	})
}
