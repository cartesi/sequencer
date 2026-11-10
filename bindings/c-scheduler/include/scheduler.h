/* (c) Cartesi and individual authors (see AUTHORS) */
/* SPDX-License-Identifier: Apache-2.0 (see LICENSE) */

#ifndef SCHEDULER_H
#define SCHEDULER_H

#include <stdint.h>

#include "application-engine.h"

/// @file
/// The C API of the canonical scheduler, a proposal to settle with the C port's author. It
/// mirrors sequencer-core's Scheduler: it turns an application's L1 inputs, in InputBox order,
/// into the one sequence of L2 transactions the application executes. The rules it carries out
/// are docs/protocol/scheduler-semantics.md; this header states only the calls.
///
/// The scheduler never calls the application and reads nothing from it. The fridge admits a
/// direct by its sender and payload length, both from the configuration, and a user op is handed
/// out once its signer recovers and its max fee covers the frame's price. Whether a handed-out op
/// executes, its nonce, is the caller's check against the application (application-engine.h).
/// No callback crosses: the caller feeds one input, then pulls its transactions one at a time.
///
/// The transaction records are application-engine.h's own, so a guest hands them straight to
/// the engine. The conventions are that header's: fixed widths and nothing as size_t, plain C
/// layout, append-only vocabularies whose values are never reused, a C enum mirrored as a 32-bit
/// integer, no exception crossing (SCHEDULER_NOEXCEPT), and errno-style failures whose text
/// travels in scheduler_get_last_error_message. Statuses are the contract, messages are
/// diagnostics, never branch on their text.
///
/// Ownership. A handle belongs to its caller from create or resume until scheduler_destroy. It
/// may move between threads, but two threads never use it at once. Every record a caller passes
/// in is borrowed for the call alone: the scheduler copies what it keeps. Every record it writes
/// out is the caller's, except the byte spans inside a SchedulerTx, which borrow the handle's
/// storage until the next scheduler_next_tx, scheduler_feed or scheduler_destroy on it.
///
/// Failure. The reference halts on an impossible state: an input out of order, a feed while the
/// previous input still has transactions, or a counter with no successor. Here such a call
/// returns a negative status instead, and a negative status from scheduler_feed or
/// scheduler_next_tx leaves the handle dead: every later call on it returns
/// SCHEDULER_STATUS_MISUSE, and only scheduler_destroy is still meaningful. Any failure kills
/// the handle, so no implementation has to prove which failures left its state whole.

/// @brief Marks the public C API, exported even when the consumer builds with
/// -fvisibility=hidden. Carried by declarations only, definitions inherit it.
#if defined(__GNUC__) || defined(__clang__)
#define SCHEDULER_API __attribute__((visibility("default")))
#else
#define SCHEDULER_API
#endif

/// @brief Spells the non-throwing guarantee for a C++ consumer, C has no equivalent.
#ifdef __cplusplus
#define SCHEDULER_NOEXCEPT noexcept
#else
#define SCHEDULER_NOEXCEPT
#endif

/// @brief The version of this header, which the implementation reports back to its caller.
/// @details Raised whenever a record's layout or a function's meaning changes. It moves apart
/// from APPLICATION_ENGINE_ABI_VERSION, though this header reuses that header's records.
#define SCHEDULER_ABI_VERSION 1

#ifdef __cplusplus
extern "C" {
#endif

/// @brief Status returned by every fallible entry point. Zero is success, DONE is the one other
/// non-failure, and every failure is negative, so `status < 0` is the failure test.
/// @details Append only.
typedef enum SchedulerStatus {
    /// Call succeeded.
    SCHEDULER_STATUS_OK = 0,
    /// scheduler_next_tx: the input has no more transactions.
    SCHEDULER_STATUS_DONE = 1,
    /// A call against the contract: out of order, a null or oversized argument, a bad
    /// configuration, or a dead handle.
    SCHEDULER_STATUS_MISUSE = -1,
    /// The L1 input count or the batch nonce has no successor. The reference halts here too, so
    /// the caller halts.
    SCHEDULER_STATUS_NO_SUCCESSOR = -2,
    /// scheduler_resume: the state and the queued range disagree. The message names the numbers.
    SCHEDULER_STATUS_RESUME_REFUSED = -3,
    /// A failure the reference scheduler has no name for, an impossible state.
    SCHEDULER_STATUS_INTERNAL_ERROR = -4,
} SchedulerStatus;

/// @brief Deployment constants, compiled into the template. None has a default: a build that
/// forgot one would run another deployment's rules.
typedef struct SchedulerConfig {
    /// The batch submitter: its inputs are batches, every other sender's are directs.
    ApplicationEngineEthereumAddress sequencer_address;
    uint64_t max_wait_blocks;          ///< The staleness window, at least 1.
    uint64_t fridge_bound;             ///< The fridge is full at this many directs.
    uint64_t max_direct_payload_bytes; ///< A direct with a longer payload never joins the fridge.
} SchedulerConfig;

/// @brief One InputBox input, as EvmAdvance delivers it.
typedef struct SchedulerInput {
    uint64_t index;                                ///< The application's InputBox index.
    ApplicationEngineEthereumAddress sender;       ///< The L1 authenticated sender.
    uint64_t block_number;                         ///< The L1 inclusion block number.
    uint64_t timestamp;                            ///< The inclusion block's Unix time, in seconds.
    uint64_t chain_id;                             ///< With app_contract, the ops' signing domain.
    ApplicationEngineEthereumAddress app_contract; ///< The application contract.
    ApplicationEngineByteSpan payload;             ///< The raw input payload.
} SchedulerInput;

/// @brief All that carries over between inputs, except the queued directs' bytes, which L1
/// holds. With the application's L2 tx count, it is the machine's `scheduler` export.
typedef struct SchedulerState {
    uint64_t l1_input_count;     ///< Inputs consumed: the position in the InputBox.
    uint64_t next_batch_nonce;   ///< The nonce the next accepted batch carries.
    uint64_t clock;              ///< The latest block the scheduler has executed at.
    uint64_t first_queued_input; ///< The oldest queued direct; l1_input_count when none is.
    uint64_t fridge_len;         ///< How many directs are queued.
} SchedulerState;

/// @brief What became of one input.
/// @details Append only. Every rejected or stale batch has the same effect, none; the reject
/// reason is a diagnostic.
typedef enum SchedulerInputOutcomeKind {
    SCHEDULER_DIRECT_QUEUED = 0,  ///< The direct joined the fridge.
    SCHEDULER_DIRECT_DROPPED = 1, ///< Its payload is over the cap: it never executes.
    SCHEDULER_BATCH_ACCEPTED = 2, ///< The batch's frames follow, and its nonce is spent.
    SCHEDULER_BATCH_STALE = 3,    ///< Its first frame is older than the staleness window.
    SCHEDULER_BATCH_REJECTED = 4, ///< A gate refused it, read `reject_reason`.
} SchedulerInputOutcomeKind;

/// @brief Why a batch was rejected.
/// @details Append only. It selects which member of SchedulerBatchRejectValues carries values.
typedef enum SchedulerBatchRejectReason {
    SCHEDULER_REJECT_DECODE_FAILED = 0,                         ///< Not an SSZ batch, no values.
    SCHEDULER_REJECT_WRONG_NONCE = 1,                           ///< Read `wrong_nonce`.
    SCHEDULER_REJECT_SAFE_BLOCK_NOT_BELOW_INCLUSION_BLOCK = 2,  ///< No values.
    SCHEDULER_REJECT_NON_MONOTONIC_SAFE_BLOCKS = 3,             ///< No values.
    SCHEDULER_REJECT_FEE_PRICE_OUT_OF_RANGE = 4,                ///< No values.
    SCHEDULER_REJECT_FIRST_FRAME_BELOW_CLOCK = 5,               ///< Read `first_frame_below_clock`.
} SchedulerBatchRejectReason;

/// @brief Values for SCHEDULER_REJECT_WRONG_NONCE.
typedef struct SchedulerWrongNonce {
    uint64_t expected; ///< The scheduler's next batch nonce.
    uint64_t got;      ///< The nonce the batch carried.
} SchedulerWrongNonce;

/// @brief Values for SCHEDULER_REJECT_FIRST_FRAME_BELOW_CLOCK.
typedef struct SchedulerFirstFrameBelowClock {
    uint64_t safe_block; ///< The first frame's safe block.
    uint64_t clock;      ///< The scheduler's clock, above it.
} SchedulerFirstFrameBelowClock;

/// @brief A rejection's values, read through the member its reason selects.
typedef union SchedulerBatchRejectValues {
    SchedulerWrongNonce wrong_nonce;                         ///< SCHEDULER_REJECT_WRONG_NONCE.
    SchedulerFirstFrameBelowClock first_frame_below_clock;   ///< SCHEDULER_REJECT_FIRST_FRAME_BELOW_CLOCK.
} SchedulerBatchRejectValues;

/// @brief An input's outcome, written whole and only when scheduler_feed returns OK.
/// @details `reject_reason` and `reject_values` mean something only for
/// SCHEDULER_BATCH_REJECTED; for any other kind they are zero.
typedef struct SchedulerInputOutcome {
    SchedulerInputOutcomeKind kind;           ///< What became of the input.
    SchedulerBatchRejectReason reject_reason; ///< Why a rejected batch was rejected.
    SchedulerBatchRejectValues reject_values; ///< The values that reason selects.
} SchedulerInputOutcome;

/// @brief The two kinds of L2 transaction.
/// @details Append only. It selects which member of SchedulerTxValues is written.
typedef enum SchedulerTxKind {
    SCHEDULER_TX_DIRECT = 0,  ///< A direct input, read `direct`.
    SCHEDULER_TX_USER_OP = 1, ///< A user op, read `user_op`.
} SchedulerTxKind;

/// @brief A transaction's body, the engine's own record for its kind.
typedef union SchedulerTxValues {
    /// SCHEDULER_TX_DIRECT, which always executes.
    ApplicationEngineDirectInput direct;
    /// SCHEDULER_TX_USER_OP, which executes only at its sender's next nonce, the caller's check.
    ApplicationEngineUserOpTx user_op;
} SchedulerTxValues;

/// @brief One handed-out transaction, written whole and only when scheduler_next_tx returns OK.
/// @details Its byte spans borrow the handle until the next scheduler_next_tx, scheduler_feed or
/// scheduler_destroy on it, so a caller that keeps the bytes copies them first.
typedef struct SchedulerTx {
    SchedulerTxKind kind; ///< Which member of `values` is written.
    /// The scheduler's next batch nonce when it handed this out: the batch whose frame holds or
    /// drained it, or the next batch to come.
    uint64_t batch_nonce;
    SchedulerTxValues values; ///< The transaction.
} SchedulerTx;

/// @brief The scheduler behind the handle, opaque to every caller.
typedef struct Scheduler Scheduler;

/// @brief The header version the implementation was built against.
/// @returns SCHEDULER_ABI_VERSION as the implementation's build saw it.
/// @details Pure and infallible. A caller checks it before anything else, since a changed
/// meaning is not a link error.
SCHEDULER_API uint32_t scheduler_abi_version(void) SCHEDULER_NOEXCEPT;

/// @brief The message describing the most recent failure on this thread.
/// @returns A NUL terminated string, never null, empty when the last fallible call succeeded.
/// @details Every fallible entry point clears it on entry and the next one overwrites it, so
/// copy rather than retain the pointer. The storage is thread local.
SCHEDULER_API const char *scheduler_get_last_error_message(void) SCHEDULER_NOEXCEPT;

/// @brief Create a scheduler at genesis: nothing consumed, nothing queued, the clock at 0.
/// @param config The deployment constants, copied.
/// @param out_scheduler The new handle, written only on OK and left untouched otherwise.
/// @returns OK, or MISUSE for a null argument or a max_wait_blocks of 0.
SCHEDULER_API SchedulerStatus scheduler_create(const SchedulerConfig *config,
    Scheduler **out_scheduler) SCHEDULER_NOEXCEPT;

/// @brief Resume a scheduler from an exported state.
/// @param config The deployment constants, copied.
/// @param state The exported state.
/// @param queued Every input from state->first_queued_input up to state->l1_input_count, in
/// order, batches and dropped directs included. Null only when queued_count is 0.
/// @param queued_count How many records `queued` holds.
/// @param out_scheduler The new handle, written only on OK and left untouched otherwise.
/// @returns OK; RESUME_REFUSED for a state whose first_queued_input is past its
/// l1_input_count, or for a range out of sequence, short or long, whose blocks decrease, whose
/// first input is not an admitted direct, with a queued direct at or below the clock, or that
/// admits another number of directs than fridge_len; or MISUSE as for create, or for a null span
/// or a count this host cannot address.
/// @details One call, so a refused range never yields a handle. The scheduler re-admits the
/// directs among the range by the same rule and copies them; the caller keeps the range.
SCHEDULER_API SchedulerStatus scheduler_resume(const SchedulerConfig *config,
    const SchedulerState *state, const SchedulerInput *queued, uint64_t queued_count,
    Scheduler **out_scheduler) SCHEDULER_NOEXCEPT;

/// @brief Destroy a scheduler, live or dead. Null is a no-op.
SCHEDULER_API void scheduler_destroy(Scheduler *scheduler) SCHEDULER_NOEXCEPT;

/// @brief Read the scheduler's state.
/// @param out_state Written only on OK.
/// @returns OK, or MISUSE for a null argument or a dead handle, which this call leaves as it was.
/// @details A feed decides everything at once (gates, drains, the clock, the nonce), so the
/// state already describes the scheduler after the last input, even while that input still has
/// transactions to hand out.
SCHEDULER_API SchedulerStatus scheduler_state(const Scheduler *scheduler,
    SchedulerState *out_state) SCHEDULER_NOEXCEPT;

/// @brief Take the next input and say what became of it. Its transactions follow from
/// scheduler_next_tx.
/// @param input Its index must be the state's l1_input_count. Borrowed for the call.
/// @param out_outcome Written whole and only on OK.
/// @returns OK; MISUSE for an index other than l1_input_count, for a feed before
/// scheduler_next_tx has returned DONE for the previous input, or for a null argument;
/// NO_SUCCESSOR when the input count, or the nonce of a batch that would be accepted, is
/// UINT64_MAX; or INTERNAL_ERROR. Every negative status leaves the handle dead.
/// @details The reference lets a feed follow an input whose transactions ran out unseen; this
/// API asks for the DONE, which the canonical loop always reads.
SCHEDULER_API SchedulerStatus scheduler_feed(Scheduler *scheduler, const SchedulerInput *input,
    SchedulerInputOutcome *out_outcome) SCHEDULER_NOEXCEPT;

/// @brief Hand out the current input's next transaction, in execution order.
/// @param out_tx Written whole and only on OK. Its spans borrow the handle until the next
/// scheduler_next_tx, scheduler_feed or scheduler_destroy on it.
/// @returns OK with a transaction; DONE when the input has no more, which it keeps returning
/// until the next feed, and before the first; MISUSE for a null argument; or INTERNAL_ERROR.
/// Every negative status leaves the handle dead.
/// @details A user op whose signature does not recover, or whose max fee is below its frame's
/// price, is never handed out; every other op is, whatever its nonce.
SCHEDULER_API SchedulerStatus scheduler_next_tx(Scheduler *scheduler,
    SchedulerTx *out_tx) SCHEDULER_NOEXCEPT;

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* SCHEDULER_H */
