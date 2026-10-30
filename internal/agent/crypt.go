package agent

import (
	"crypto/aes"
	"crypto/cipher"
	"crypto/rand"
	"encoding/base64"
	"fmt"
	"os"
	"path/filepath"
)

// Turn-text encryption at rest (AES-256-GCM, stdlib only).
//
// Every newly written turn is sealed before INSERT; ListTurns opens rows on
// read, so the agent loop, export, and CLI all see plaintext transparently.
// Rows without the version prefix are legacy plaintext and read as-is, so
// existing databases keep working and are encrypted on next write path
// (new turns; old rows re-encrypt when copied, e.g. by ForkSession).
//
// Threat model: protects the DB file against offline theft, backups, and
// casual inspection. It does NOT hide text from the LLM provider (the model
// must see plaintext to reason) nor from another process running as the same
// user (the key lives beside the DB by design).

// cryptPrefix versions the sealed envelope; bump when the format changes.
const cryptPrefix = "v1:"

// keyFileName sits next to the DB it encrypts (0600).
const keyFileName = ".lumina-turnkey"

// loadOrCreateKey reads the 32-byte data key beside dbPath, minting and
// persisting one (0600) on first open.
func loadOrCreateKey(dbPath string) ([]byte, error) {
	p := filepath.Join(filepath.Dir(dbPath), keyFileName)
	if raw, err := os.ReadFile(p); err == nil {
		if len(raw) != 32 {
			return nil, fmt.Errorf("turnkey %s: want 32 bytes, got %d", p, len(raw))
		}
		return raw, nil
	} else if !os.IsNotExist(err) {
		return nil, err
	}
	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		return nil, err
	}
	// ponytail: 0600 via OpenFile, not write+chmod (no window of exposure).
	f, err := os.OpenFile(p, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return nil, err
	}
	if _, err := f.Write(key); err != nil {
		_ = f.Close()
		return nil, err
	}
	if err := f.Close(); err != nil {
		return nil, err
	}
	return key, nil
}

func sealGCM(key, plain []byte) (string, error) {
	block, err := aes.NewCipher(key)
	if err != nil {
		return "", err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return "", err
	}
	nonce := make([]byte, gcm.NonceSize())
	if _, err := rand.Read(nonce); err != nil {
		return "", err
	}
	out := gcm.Seal(nonce, nonce, plain, nil)
	return cryptPrefix + base64.StdEncoding.EncodeToString(out), nil
}

func openGCM(key []byte, sealed string) (string, error) {
	raw, err := base64.StdEncoding.DecodeString(sealed[len(cryptPrefix):])
	if err != nil {
		return "", err
	}
	block, err := aes.NewCipher(key)
	if err != nil {
		return "", err
	}
	gcm, err := cipher.NewGCM(block)
	if err != nil {
		return "", err
	}
	n := gcm.NonceSize()
	if len(raw) < n {
		return "", fmt.Errorf("sealed turn too short")
	}
	plain, err := gcm.Open(nil, raw[:n], raw[n:], nil)
	if err != nil {
		return "", err
	}
	return string(plain), nil
}

// sealTurn encrypts text for storage; openTurn reverses it, passing legacy
// plaintext rows through untouched.
func sealTurn(key []byte, text string) (string, error) {
	return sealGCM(key, []byte(text))
}

func openTurn(key []byte, stored string) (string, error) {
	if len(stored) < len(cryptPrefix) || stored[:len(cryptPrefix)] != cryptPrefix {
		return stored, nil // legacy plaintext row
	}
	return openGCM(key, stored)
}
