# Durability Specification: No Committed Data Is Ever Lost

## Overview

This document specifies the properties that guarantee **no committed data is ever lost** in TursoDB. The specification is structured hierarchically from the top-level guarantee down to component-level invariants, with composition rules showing how they combine.

**Status**: All 16 component properties verified through code analysis (agent-asserted).

## Top-Level Guarantee

**Property**: After a transaction commits successfully, the data persists across process crashes, system crashes, and power failures.

**Acceptance Criteria**:
1. Given a committed transaction T, when crash occurs at any point after commit, then all data from T is recoverable
2. Given a committed transaction T, when recovery completes, then reading any key written by T returns the committed value  
3. Given an uncommitted transaction T, when crash occurs, then no partial data from T is visible after recovery

## Data Flow Architecture

```
User Data
    │
    ▼
┌─────────────────┐
│ Record Encoding │  Property 15: Serialization bijection
│   (serialize)   │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│    B-tree       │  Properties 1-3: No lost/duplicated cells, sort order
│  (pages/cells)  │  Properties 12-13: Freelist safety, overflow consistency
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│   Page Cache    │  Properties 4-6: Read consistency, no duplicates, WAL protocol
│  (dirty pages)  │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│  Dirty Tracking │  Property 7: Tracking completeness
│    (bitmap)     │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│      WAL        │  Properties 9-10: Commit completeness, atomicity
│   (frames)      │  Property 14: Checksum chain integrity
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│   Checkpoint    │  Property 11: Data integrity
│  (to DB file)   │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│    Recovery     │  Property 8: Recovery correctness
│  (WAL replay)   │  Property 16: Header consistency
└─────────────────┘
```

## Component Properties

### B-tree Layer (Properties 1-3, 12-13)

| # | Property | Summary | File |
|---|----------|---------|------|
| 1 | `btree.balance_no_lost_nodes` | Balancing never loses cells | btree.rs |
| 2 | `btree.balance_no_duplicates` | Balancing never duplicates cells | btree.rs |
| 3 | `btree.balance_sort_order` | Balancing preserves sort order | btree.rs |
| 12 | `btree.freelist_safety` | Freed pages safe from premature reuse | pager.rs |
| 13 | `btree.overflow_consistency` | Overflow chains remain consistent | btree.rs, payload.rs |

### Page Cache Layer (Properties 4-6)

| # | Property | Summary | File |
|---|----------|---------|------|
| 4 | `cache.read_consistency` | Read returns latest committed version | pager.rs |
| 5 | `cache.no_duplicates` | At most one cache entry per page | page_cache.rs |
| 6 | `cache.wal_protocol` | Dirty pages to WAL before DB file | pager.rs, wal.rs |

### Dirty Tracking (Property 7)

| # | Property | Summary | File |
|---|----------|---------|------|
| 7 | `dirty.tracking_completeness` | Every modified page tracked before commit | pager.rs |

### WAL Layer (Properties 8-10, 14)

| # | Property | Summary | File |
|---|----------|---------|------|
| 8 | `wal.recovery_correctness` | Recovery restores committed state | sqlite3_ondisk.rs |
| 9 | `wal.commit_completeness` | Every dirty page written to WAL | pager.rs |
| 10 | `wal.transaction_atomicity` | All-or-nothing visibility | wal.rs |
| 14 | `wal.checksum_integrity` | Checksum chain detects corruption | sqlite3_ondisk.rs |

### Checkpoint & Header (Properties 11, 16)

| # | Property | Summary | File |
|---|----------|---------|------|
| 11 | `checkpoint.data_integrity` | Correct data to correct file offsets | wal.rs |
| 16 | `header.consistency` | Header reflects valid committed state | pager.rs |

### Record Layer (Property 15)

| # | Property | Summary | File |
|---|----------|---------|------|
| 15 | `record.serialization_bijection` | Serialize/deserialize round-trips | record.rs |

## Negative Specifications (NEVER)

These specify what must **never** happen:

| Tag | Prohibition | Category |
|-----|-------------|----------|
| `never_lose_committed` | NEVER lose committed data | data_loss |
| `never_partial_transaction` | NEVER make partial transaction visible | state_corruption |
| `never_corrupt_checksum` | NEVER accept frame with invalid checksum | silent_failure |
| `never_orphan_overflow` | NEVER leave overflow pages unreachable | resource_leak |
| `never_reuse_active_page` | NEVER allocate page still referenced by B-tree | state_corruption |

## Composition Rule

The top-level durability guarantee is achieved by composing all 16 component properties:

1. **Record Layer**: User data correctly serialized (bijection)
2. **B-tree Layer**: Operations preserve data (no loss, no duplicates, order preserved)
3. **Freelist**: Safe page lifecycle (no premature reuse)
4. **Overflow**: Chain integrity maintained
5. **Cache**: Consistent view, respects WAL protocol
6. **Dirty Tracking**: All modifications captured
7. **WAL Commit**: All dirty pages reach WAL
8. **Atomicity**: All-or-nothing transaction visibility
9. **Checksum**: Corruption detection
10. **Checkpoint**: Correct persistence to DB file
11. **Recovery**: Restores exactly committed state
12. **Header**: Consistent with data after recovery

If any component fails, the durability guarantee is broken.

## Verification Status

| Layer | Properties | Verified |
|-------|------------|----------|
| Record Serialization | 1 | 1 |
| B-tree Operations | 5 | 5 |
| Page Cache | 3 | 3 |
| Dirty Tracking | 1 | 1 |
| WAL Protocol | 4 | 4 |
| Checkpoint | 1 | 1 |
| Header | 1 | 1 |
| **Total** | **16** | **16** |

**Trust Level**: All proofs are agent-asserted based on code analysis. No machine-checked verification tools were used.

## Key Code Locations

| Component | Primary File | Key Functions/Lines |
|-----------|--------------|---------------------|
| B-tree balance | btree.rs | `balance_non_root` (3045-3918) |
| Page cache | pager.rs | `read_page` (2785-2820) |
| Dirty tracking | pager.rs | `add_dirty` (2917-2934) |
| WAL commit | pager.rs | `commit_dirty_pages_inner` (3486-3736) |
| WAL atomicity | wal.rs | `prepare_collected_frames` (1980-2032) |
| Checksum | sqlite3_ondisk.rs | `checksum_wal` (1524-1560), `process_frames` (1658-1714) |
| Recovery | sqlite3_ondisk.rs | `finalize_loading` (1737-1765) |
| Checkpoint | wal.rs | checkpoint state machine (2368-2610) |
| Header | pager.rs | `HeaderRefMut::from_pager` (92-97) |
| Record | record.rs | `serialize`, `deserialize` |

## Future Work

1. Formalize properties in TLA+ for model checking
2. Add property-based tests (e.g., with proptest) for each guarantee
3. Consider machine-checking critical state machines (commit, checkpoint, recovery)
4. Add fault injection tests to validate crash recovery
