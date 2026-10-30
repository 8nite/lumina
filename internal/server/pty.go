// Minimal RFC6455 text-frame loop for the PTY endpoint.
package server

import (
	"crypto/rand"
	"encoding/binary"
	"io"
	"net"
	"os/exec"
)

// wsServeLoop reads masked client text frames, executes each line via sh,
// and writes back unmasked text frames with combined output.
func wsServeLoop(conn net.Conn) {
	for {
		opcode, payload, err := wsReadFrame(conn)
		if err != nil {
			return
		}
		if opcode == 0x8 { // close
			return
		}
		if opcode != 0x1 && opcode != 0x2 {
			continue
		}
		line := string(payload)
		if len(line) == 0 {
			continue
		}
		out, _ := exec.Command("sh", "-c", line).CombinedOutput()
		if len(out) == 0 {
			out = []byte("(exit ok, no output)")
		}
		if err := wsWriteFrame(conn, 0x1, out); err != nil {
			return
		}
	}
}

func wsReadFrame(r io.Reader) (byte, []byte, error) {
	var hdr [2]byte
	if _, err := io.ReadFull(r, hdr[:]); err != nil {
		return 0, nil, err
	}
	opcode := hdr[0] & 0x0F
	masked := hdr[1]&0x80 != 0
	length := int(hdr[1] & 0x7F)
	switch length {
	case 126:
		var ext [2]byte
		if _, err := io.ReadFull(r, ext[:]); err != nil {
			return 0, nil, err
		}
		length = int(binary.BigEndian.Uint16(ext[:]))
	case 127:
		var ext [8]byte
		if _, err := io.ReadFull(r, ext[:]); err != nil {
			return 0, nil, err
		}
		length = int(binary.BigEndian.Uint64(ext[:]))
	}
	var mask [4]byte
	if masked {
		if _, err := io.ReadFull(r, mask[:]); err != nil {
			return 0, nil, err
		}
	}
	payload := make([]byte, length)
	if _, err := io.ReadFull(r, payload); err != nil {
		return 0, nil, err
	}
	if masked {
		for i := range payload {
			payload[i] ^= mask[i%4]
		}
	}
	return opcode, payload, nil
}

func wsWriteFrame(w io.Writer, opcode byte, payload []byte) error {
	var hdr []byte
	hdr = append(hdr, 0x80|opcode)
	n := len(payload)
	switch {
	case n < 126:
		hdr = append(hdr, byte(n))
	case n < 65536:
		hdr = append(hdr, 126, byte(n>>8), byte(n))
	default:
		hdr = append(hdr, 127, 0, 0, 0, 0, byte(n>>24), byte(n>>16), byte(n>>8), byte(n))
	}
	_ = rand.Read
	if _, err := w.Write(hdr); err != nil {
		return err
	}
	_, err := w.Write(payload)
	return err
}
