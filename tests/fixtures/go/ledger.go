// Package ledger is a small fixture exercising the Go symbol extractor.
package ledger

import "fmt"

// Account holds a balance for a named owner.
type Account struct {
	Owner   string
	Balance int64
}

// Ledger records and applies transactions.
type Ledger interface {
	Apply(a *Account, delta int64) error
	String() string
}

// memLedger is an in-memory Ledger implementation.
type memLedger struct {
	entries int
}

// NewLedger constructs an in-memory ledger.
func NewLedger() Ledger {
	return &memLedger{}
}

// Apply adjusts an account balance by delta.
func (m *memLedger) Apply(a *Account, delta int64) error {
	a.Balance += delta
	m.entries++
	return nil
}

// String renders the ledger summary.
func (m memLedger) String() string {
	return fmt.Sprintf("ledger(%d entries)", m.entries)
}
