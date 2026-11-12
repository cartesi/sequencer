/* (c) Cartesi and individual authors (see AUTHORS) */
/* SPDX-License-Identifier: Apache-2.0 (see LICENSE) */

#ifndef APPLICATION_ENGINE_H
#define APPLICATION_ENGINE_H

#include <stdint.h>

/// @file
/// The C API of an application engine, and the only surface an engine exports. It mirrors the
/// Cartesi sequencer's Application contract, so the host running the engine stays application
/// agnostic and an engine is swappable behind this header. Plain C so any host can consume it.
///
/// An application implements these declarations into a static archive, which the `c-app-engine`
/// shim links at build time to become the sequencer's Application. The same engine implementation
/// can be compiled for the native sequencer and the application's canonical target; deterministic,
/// equivalent behavior across those builds remains the application's responsibility. An engine
/// written in C++, or in any language with a C ABI, implements it the same way.
///
/// Nothing application specific crosses. An engine is handed a dump already holding a configured
/// deployment, so a host never learns what configures the application it runs, and the path is
/// opaque, a file or a directory as the engine chooses.
///
/// The Rust host generates its declarations from this header with bindgen. The supplied header
/// must match the linked archive; binding generation does not verify that pairing.
/// The records that cross are plain C layout and the
/// engine must static_assert their sizes and field offsets, so a compiler laying one out
/// differently fails its build rather than the seam. A generated binding carries the same checks
/// on the host side.
///
/// A generated binding follows whatever this header says, so the header carries the compatibility
/// obligation on its own. Every vocabulary here is append only and no value is ever reused, since
/// a host is entitled to refuse a value it does not know rather than to have it renumbered
/// underneath.
///
/// The header is versioned by APPLICATION_ENGINE_ABI_VERSION, and the engine reports the version
/// it was built against through application_engine_abi_version. A host checks it before any other
/// call: a function whose meaning changed, or a record that grew, links against an older archive
/// without complaint, and only the version tells them apart. A header without the version is
/// version 1.
///
/// Widths are fixed, nothing crosses as size_t. A C enum's underlying type is implementation
/// defined, so an engine must static_assert each one's width against int32_t, which is what lets
/// a host mirror them as a plain 32-bit integer.
///
/// Boundary rules, binding on every function here. Every entry point is total over the bytes it
/// is handed: a payload arrives from whoever signed or posted it, so a method an engine cannot
/// parse is a refusal or a counted no-op, never INTERNAL_ERROR. Reporting INTERNAL_ERROR there
/// would hand any caller a process kill, since it is fatal-no-resume.
///
/// No exception may cross. Fallible entry points report errors through status codes; a fatal
/// admission, nonce read, or execution error defines no successor and the caller must discard the
/// instance. IO_ERROR is legal from every status-returning entry point. In admission, nonce reads,
/// execution, and output draining it is fatal for the instance; the host may retry with a fresh
/// instance after an operational failure. NOT_FOUND and INVALID_DUMP describe lifecycle failures
/// only.
///
/// Execution is consensus and admission is not. State transitions, nonces, the L2 tx count, and
/// outputs must agree with the canonical application. Admission is the operator's policy, asked
/// only by the sequencer before it takes an op in, and the canonical machine never calls it, so a
/// canonical build need not implement it. Refusal diagnostics and error messages need not be
/// identical across builds.
///
/// Nonces follow one rule, which the caller relies on to decide which user ops run: a genesis
/// state starts every account at 0, application_engine_user_nonce reports the sender's next nonce,
/// each executed user op advances its sender's nonce by exactly one, and nothing else changes a
/// nonce. The engine stores nonces and never compares them. The caller reads the sender's next
/// nonce, skips an op whose nonce differs from it or equals UINT32_MAX, which has no successor, and
/// executes every other op, so an engine decides what an op does but never whether it runs. See
/// docs/protocol/application-contract.md.
///
/// Fees are uint16_t exponents with base 129/128, denominated in the fee token's smallest unit.
/// The conversion is defined by sequencer-core/src/fee.rs and its build.rs-generated table:
/// integer fixed-point arithmetic with 64 fractional bits, including its rounding and exponent
/// bound. Native and canonical implementations must agree on this conversion. The max-fee guard
/// compares exponents and belongs to the caller. Admission and execution use the converted amount:
/// admission may refuse an op its sender cannot pay in full, while execution charges what the
/// sender has, up to the fee, and never fails over it.
///
/// Errors are errno style. A fallible entry point returns a status (or a null pointer for
/// application_engine_state_file_in_dump) and leaves the reason for
/// application_engine_get_last_error_message. Statuses are the contract, messages are diagnostics,
/// never branch on their text.

/// @brief Marks the public C API, exported even when the consumer builds with
/// -fvisibility=hidden. Carried by declarations only, definitions inherit it.
#if defined(__GNUC__) || defined(__clang__)
#define APPLICATION_ENGINE_API __attribute__((visibility("default")))
#else
#define APPLICATION_ENGINE_API
#endif

/// @brief Spells the non-throwing guarantee for a C++ consumer, C has no equivalent.
/// @details Part of every signature, a throw reaching the seam terminates rather than unwinding
/// into a caller that has no way to handle it.
#ifdef __cplusplus
#define APPLICATION_ENGINE_NOEXCEPT noexcept
#else
#define APPLICATION_ENGINE_NOEXCEPT
#endif

/// @brief Size in bytes of an account address crossing the seam.
/// @details An engine must static_assert it against its own address type, so the two cannot
/// drift.
#define APPLICATION_ENGINE_ADDRESS_SIZE 20

/// @brief Size in bytes of an on-chain amount crossing the seam.
/// @details An amount is an EVM word, wider than any scalar this API carries, so it crosses as
/// raw big-endian bytes rather than as a number a host may not be able to spell.
#define APPLICATION_ENGINE_VALUE_SIZE 32

/// @brief The version of this header, which the engine reports back to the host.
/// @details Raised whenever a record's layout or a function's meaning changes. Version 2
/// replaced validation with admission and made user-op execution total. Version 3 added the
/// staleness window to the scheduler limits.
#define APPLICATION_ENGINE_ABI_VERSION 3

#ifdef __cplusplus
extern "C" {
#endif

/// @brief The header version the engine was built against.
/// @returns APPLICATION_ENGINE_ABI_VERSION as the engine's build saw it.
/// @details Pure and infallible. The host calls it before anything else and refuses an engine
/// reporting another version, since a changed meaning is not a link error.
APPLICATION_ENGINE_API uint32_t application_engine_abi_version(void) APPLICATION_ENGINE_NOEXCEPT;

/// @brief The largest user-op method payload, in bytes.
/// @details Pure, infallible, and constant for the linked engine implementation, independent of
/// any loaded deployment. Zero permits only empty method payloads. The host uses this value for
/// ingress admission and batch sizing; it does not bound direct inputs from L1.
APPLICATION_ENGINE_API uint64_t application_engine_max_method_payload_bytes(void) APPLICATION_ENGINE_NOEXCEPT;

/// @brief The scheduler constants a template compiles in for this application.
/// @details They belong to the application: the window is how long it lets a sequencer hold a
/// direct back, and the other two bound what its directs may cost the canonical machine. All
/// three are consensus values: a template's scheduler and every host must read the same ones.
typedef struct ApplicationEngineSchedulerLimits {
    /// The staleness window in L1 blocks, at least 1: a direct queued this long is executed, and
    /// a batch whose first frame is this old is skipped.
    uint64_t max_wait_blocks;
    uint64_t fridge_bound;             ///< How many admitted directs the fridge may hold.
    uint64_t max_direct_payload_bytes; ///< A direct with a longer payload never executes.
} ApplicationEngineSchedulerLimits;

/// @brief Read the scheduler constants for this application.
/// @param out_limits Writable record, written whole before returning.
/// @details Pure, infallible, and constant for the linked engine implementation, independent of
/// any loaded deployment, like the method payload bound.
APPLICATION_ENGINE_API void application_engine_scheduler_limits(
    ApplicationEngineSchedulerLimits *out_limits) APPLICATION_ENGINE_NOEXCEPT;

/// @brief An account address, raw bytes and no encoding.
/// @details A named type rather than a loose buffer, so an address and an amount cannot be
/// passed for one another.
typedef struct ApplicationEngineEthereumAddress {
    uint8_t bytes[APPLICATION_ENGINE_ADDRESS_SIZE]; ///< The address bytes, in on-chain order.
} ApplicationEngineEthereumAddress;

/// @brief A 256-bit unsigned amount, big-endian.
/// @details The width every on-chain amount crosses at, whatever the engine prices it in
/// internally, so no amount is narrowed to fit through here.
typedef struct ApplicationEngineUint256 {
    uint8_t bytes[APPLICATION_ENGINE_VALUE_SIZE]; ///< The amount bytes, most significant first.
} ApplicationEngineUint256;

/// @brief A borrowed range of bytes, never owned by the side receiving it.
/// @details Const because nothing crossing here is written through, inputs and output payloads
/// alike are read only for whoever receives them.
typedef struct ApplicationEngineByteSpan {
    const uint8_t *data; ///< The first byte, null only when the range is empty.
    uint64_t size;       ///< The number of bytes.
} ApplicationEngineByteSpan;

/// @brief Status returned by every fallible entry point. Zero is success and every failure is
/// negative, so `status < 0` is the failure test and a value added later never disturbs it.
/// @details Append only. The status is the contract, the accompanying message is a diagnostic.
typedef enum ApplicationEngineStatus {
    APPLICATION_ENGINE_STATUS_OK = 0,              ///< Call succeeded.
    APPLICATION_ENGINE_STATUS_INVALID = -1,        ///< Refused by admission.
    APPLICATION_ENGINE_STATUS_INTERNAL_ERROR = -2, ///< Internal engine failure, fatal-no-resume.
    APPLICATION_ENGINE_STATUS_IO_ERROR = -3,       ///< An operational filesystem or mapping failure.
    APPLICATION_ENGINE_STATUS_NOT_FOUND = -4,      ///< A required dump artifact is missing.
    APPLICATION_ENGINE_STATUS_INVALID_DUMP = -5,   ///< A dump is malformed or truncated.
} ApplicationEngineStatus;

/// @brief A user op as its sender signed it, the record admission reads.
/// @details The engine carries the nonce and max_fee without checking either, those guards
/// belong to the caller, so an op arrives whole.
typedef struct ApplicationEngineUserOp {
    uint32_t nonce;                 ///< Sender replay protection nonce.
    uint16_t max_fee;               ///< Highest frame fee exponent the sender accepts, base 129/128.
    ApplicationEngineByteSpan data; ///< Method payload, opaque here and parsed by the engine.
} ApplicationEngineUserOp;

/// @brief A user op as the scheduler hands it out, the record execution reads.
/// @details The caller found its nonce equal to the sender's next one before handing it over, so
/// execution advances that nonce by exactly one without comparing. Its max fee already covered
/// the frame's price, leaving the fee the frame charges.
typedef struct ApplicationEngineUserOpTx {
    ApplicationEngineEthereumAddress sender; ///< The recovered signer.
    uint32_t nonce;                          ///< The nonce the sender signed, the sender's next.
    uint16_t fee;                            ///< The charged frame fee exponent, base 129/128.
    uint64_t safe_block;                     ///< The safe block of the frame carrying the op.
    ApplicationEngineByteSpan data;          ///< Method payload, opaque here and parsed by the engine.
} ApplicationEngineUserOpTx;

/// @brief An input taken straight from the L1 input box.
/// @details Its sender is authenticated by the chain rather than recovered from a signature.
/// Its payload is still untrusted application input; the caller never hands over one longer
/// than the scheduler limits' max_direct_payload_bytes.
typedef struct ApplicationEngineDirectInput {
    ApplicationEngineEthereumAddress sender; ///< The L1 authenticated sender.
    uint64_t input_index;                    ///< The application's input box index of the input.
    uint64_t block_number;                   ///< The L1 inclusion block number.
    uint64_t timestamp;                      ///< The L1 inclusion block's Unix time, in seconds.
    ApplicationEngineByteSpan payload;       ///< The raw input payload.
} ApplicationEngineDirectInput;

/// @brief Why admission refused a user op.
/// @details Append only. Written only on APPLICATION_ENGINE_STATUS_INVALID, and it selects which
/// member of ApplicationEngineInvalidValues carries the diagnostics. Admission produces only
/// INSUFFICIENT_FEE_BALANCE and REFUSED_BY_POLICY; the others are retired, since their checks
/// belong to the caller, and keep their numbers so none is ever reused.
typedef enum ApplicationEngineInvalidReason {
    APPLICATION_ENGINE_INVALID_NONCE = 0,            ///< Retired, the caller owns the nonce check.
    APPLICATION_ENGINE_INVALID_MAX_FEE = 1,          ///< Retired, the caller owns the max fee guard.
    APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE = 2, ///< Cannot cover the frame fee, read `fee_balance`.
    APPLICATION_ENGINE_NONCE_EXHAUSTED = 3,          ///< Retired, the caller owns the UINT32_MAX guard.
    APPLICATION_ENGINE_REFUSED_BY_POLICY = 4,        ///< Any other operator policy, no values.
} ApplicationEngineInvalidReason;

/// @brief Diagnostics for the retired APPLICATION_ENGINE_INVALID_NONCE.
typedef struct ApplicationEngineInvalidNonce {
    uint32_t expected; ///< The nonce the account expects next.
    uint32_t got;      ///< The nonce the op carried.
} ApplicationEngineInvalidNonce;

/// @brief Diagnostics for the retired APPLICATION_ENGINE_INVALID_MAX_FEE.
/// @details Both values are log space exponents, base 129/128. No engine produces this reason,
/// the guard belongs to the caller, it is carried so the reason vocabulary stays whole.
typedef struct ApplicationEngineInvalidMaxFee {
    uint16_t max_fee;  ///< The highest frame fee price the sender accepts.
    uint16_t base_fee; ///< The frame fee price charged.
} ApplicationEngineInvalidMaxFee;

/// @brief Diagnostics for APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE.
/// @details Both amounts are in the smallest unit of whatever the engine charges fees in. An
/// all-ones required means a fee no balance could ever cover, an engine reports it that way
/// rather than reporting an amount it cannot represent.
typedef struct ApplicationEngineInsufficientFeeBalance {
    ApplicationEngineUint256 required;  ///< The frame fee the sender must cover.
    ApplicationEngineUint256 available; ///< What the sender has free to cover it.
} ApplicationEngineInsufficientFeeBalance;

/// @brief The diagnostics of a refusal, read through the member its reason selects.
/// @details APPLICATION_ENGINE_REFUSED_BY_POLICY has no member: its explanation is text, which
/// travels in the last error message.
typedef union ApplicationEngineInvalidValues {
    ApplicationEngineInvalidNonce nonce;                 ///< The retired APPLICATION_ENGINE_INVALID_NONCE.
    ApplicationEngineInvalidMaxFee max_fee;              ///< The retired APPLICATION_ENGINE_INVALID_MAX_FEE.
    ApplicationEngineInsufficientFeeBalance fee_balance; ///< APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE.
} ApplicationEngineInvalidValues;

/// @brief A refusal, its reason and the diagnostics that reason selects.
/// @details Written whole and only on APPLICATION_ENGINE_STATUS_INVALID. Reading a member other
/// than the one the reason names is a caller bug, the unselected members hold nothing.
typedef struct ApplicationEngineInvalid {
    ApplicationEngineInvalidReason reason; ///< Why the op was refused.
    ApplicationEngineInvalidValues values; ///< The diagnostics for that reason.
} ApplicationEngineInvalid;

/// @brief The kinds of output an engine can emit, mirroring the rollup output types.
/// @details Append only. A host that meets a kind it does not know has drifted from the engine
/// and must refuse rather than guess, the kinds carry different payload meanings.
typedef enum ApplicationEngineOutputKind {
    APPLICATION_ENGINE_OUTPUT_VOUCHER = 0, ///< A call to a destination, read `voucher`.
    APPLICATION_ENGINE_OUTPUT_NOTICE = 1,  ///< A payload only attestation, read `notice`.
} ApplicationEngineOutputKind;

/// @brief A call the chain makes on the application's behalf.
typedef struct ApplicationEngineVoucher {
    ApplicationEngineEthereumAddress destination; ///< The contract to call.
    ApplicationEngineUint256 value;               ///< The call value in wei.
    ApplicationEngineByteSpan payload;            ///< The encoded call payload.
} ApplicationEngineVoucher;

/// @brief An output's body, read through the member its kind selects.
typedef union ApplicationEngineOutputValues {
    ApplicationEngineVoucher voucher; ///< APPLICATION_ENGINE_OUTPUT_VOUCHER.
    ApplicationEngineByteSpan notice; ///< APPLICATION_ENGINE_OUTPUT_NOTICE, the attested payload.
} ApplicationEngineOutputValues;

/// @brief An output an execution emitted, its kind and the body that kind selects.
/// @details Written whole and only when a drain returns OK. Reading a member other than the one
/// the kind names is a caller bug, the unselected members hold nothing.
typedef struct ApplicationEngineOutput {
    ApplicationEngineOutputKind kind;     ///< What the engine emitted.
    ApplicationEngineOutputValues values; ///< The body for that kind.
} ApplicationEngineOutput;

/// @brief The engine instance behind the handle, opaque to every caller.
typedef struct ApplicationEngine ApplicationEngine;

/// @brief Get the message describing the most recent failure.
/// @returns A NUL terminated string, never null, empty when the last fallible call succeeded.
/// @details Read it after a negative status or a null handle. Every fallible entry point clears
/// it on entry, so it always describes the call that just failed, and the next one overwrites
/// it, so copy rather than retain the pointer. Infallible entry points leave it untouched.
/// The storage is thread local, matching the ownership the seam already requires: an engine may
/// move between threads but its handle is never used by two at once.
///
/// The entry points that take no handle are the exception and must be reentrant. A host may call
/// application_engine_state_file_in_dump from request handlers while an execution is in flight,
/// so an engine answering out of one process-wide buffer would race. This message and that path
/// are both thread local here for that reason.
APPLICATION_ENGINE_API const char *application_engine_get_last_error_message(void) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Open an engine over a dump holding an existing deployment.
/// @param prefix The dump to open, carrying whatever shape the engine gives a dump.
/// @param out_engine The engine handle, written only on OK and left untouched otherwise.
/// @returns OK, NOT_FOUND for a missing artifact, INVALID_DUMP for malformed/truncated bytes,
/// IO_ERROR for other filesystem failures, or INTERNAL_ERROR for an engine invariant failure.
/// @details The only way to open an engine, and it never creates. A state is written once at
/// genesis by a tool that knows the application's configuration, and every engine afterwards opens
/// what is there, which is what keeps deployment configuration off this API.
///
/// Loading must leave the source dump immutable. Two engines may open the same dump and evolve
/// independently, and deleting the source after loading must leave both usable. Private mapping,
/// deserialization, or an independently owned working copy can implement this contract. The caller
/// never mutates a source dump underneath a live engine.
///
/// The deployment found there is validated before the handle is written, the backstop for a state
/// that was truncated, hand edited, or left by a genesis that died mid-write.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_from_dump(const char *prefix,
    ApplicationEngine **out_engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Open an engine over a canonical machine's comparison bytes, at the machine's count.
/// @param state_file The file holding the bytes the canonical machine yields for its state: its
/// whole labeled state range, or its `state` inspect report.
/// @param l2_tx_count The canonical machine's L2 tx count, which the new instance starts at
/// whether or not the bytes hold one.
/// @param out_engine The engine handle, written only on OK and left untouched otherwise.
/// @returns OK, NOT_FOUND for a missing file, INVALID_DUMP for bytes that do not decode or that
/// contradict l2_tx_count, IO_ERROR for other filesystem failures, or INTERNAL_ERROR.
/// @details How a host starts from a canonical machine rather than from one of its own dumps. A
/// dump of the result must hold the same comparison bytes in the file
/// application_engine_state_file_in_dump names. Like application_engine_from_dump it never
/// changes the source file, and the instance stays usable after the file is deleted.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_from_canonical_state(const char *state_file,
    uint64_t l2_tx_count, ApplicationEngine **out_engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Destroy an engine instance.
/// @param engine The engine handle.
APPLICATION_ENGINE_API void application_engine_destroy(ApplicationEngine *engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Read a sender's next nonce, pure and read-only.
/// @param engine The engine handle.
/// @param sender The account whose nonce to read.
/// @param out_nonce The nonce the sender's next executed op carries, written only on OK.
/// @returns OK, IO_ERROR, or INTERNAL_ERROR. An error requires discarding the instance.
/// @details Zero for an address that has sent nothing. The caller compares it with each user op
/// before executing, and checks after an execution that it advanced by exactly one.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_user_nonce(const ApplicationEngine *engine,
    const ApplicationEngineEthereumAddress *sender, uint32_t *out_nonce) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Decide whether the operator wants a user op in a frame at this fee, pure and read-only.
/// @param engine The engine handle.
/// @param sender The recovered signer.
/// @param user_op The op to admit, as its sender signed it.
/// @param fee The frame fee exponent, base 129/128.
/// @param out_invalid Why the op was refused, written whole and only on INVALID.
/// @returns OK to admit, INVALID with a reason, IO_ERROR, or INTERNAL_ERROR.
/// @details Policy, never consensus: only the sequencer asks, before it takes an op in, and the
/// answer never changes what an included op does. The nonce, the max-fee and exhausted-nonce
/// guards belong to the caller and are never checked here, so only
/// APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE and APPLICATION_ENGINE_REFUSED_BY_POLICY come back.
///
/// An INSUFFICIENT_FEE_BALANCE refusal reports itself through out_invalid alone and leaves the
/// last error message empty. A REFUSED_BY_POLICY refusal leaves its explanation in the last error
/// message, which the sequencer hands to the client verbatim. IO_ERROR and INTERNAL_ERROR carry an
/// error message. Queued outputs are left alone, only an execution touches them.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_admit_user_op(const ApplicationEngine *engine,
    const ApplicationEngineEthereumAddress *sender, const ApplicationEngineUserOp *user_op, uint16_t fee,
    ApplicationEngineInvalid *out_invalid) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Execute a user op the caller found at its sender's next nonce.
/// @param engine The engine handle.
/// @param user_op The op to execute.
/// @param out_output_count How many outputs this op left waiting, written only on OK.
/// @returns OK, IO_ERROR, or INTERNAL_ERROR. An error requires discarding the instance.
/// @details Total: whatever the op does or pays, it reports OK, advances its sender's nonce by
/// exactly one, and advances the L2 tx count by exactly one. A fee the sender cannot cover is no
/// failure: the engine charges what the sender has, up to the fee, and runs the method on what is
/// left. An op the method rejects still executed and still counts. Only the resulting state,
/// count, and outputs cross the seam; business-failure diagnostics are not returned separately.
///
/// An execution refuses to run while an earlier execution's outputs are still queued, reporting
/// INTERNAL_ERROR without executing anything rather than discarding outputs meant to reach the
/// chain. That refusal is what makes the count reported this op's own.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_execute_user_op(ApplicationEngine *engine,
    const ApplicationEngineUserOpTx *user_op, uint64_t *out_output_count) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Execute a direct input from the L1 input box.
/// @param engine The engine handle.
/// @param input The input to execute.
/// @param out_output_count How many outputs this input left waiting, written only on OK.
/// @returns OK, IO_ERROR, or INTERNAL_ERROR. An error requires discarding the instance.
/// @details An input the engine ignores or cannot parse is a counted no-op and still reports OK.
/// It advances the L2 tx count by exactly one and never changes a nonce. Outputs behave as they
/// do for a user op.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_execute_direct_input(ApplicationEngine *engine,
    const ApplicationEngineDirectInput *input, uint64_t *out_output_count) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Take the next queued output, in emission order.
/// @param engine The engine handle.
/// @param out_output The output taken, written whole and only on OK.
/// @returns OK with an output written, IO_ERROR, or INTERNAL_ERROR.
/// @details Call it exactly as many times as the execution reported, which is what attributes
/// the outputs to the input that produced them. Taking one more than were queued is a caller bug
/// and reports INTERNAL_ERROR rather than an empty output a host might act on. The payload
/// pointer stays valid until the next drain call releases it, so copy before draining again.
/// A voucher carries its value even when an engine only ever emits zero-value ones, so a host
/// reads what the engine emitted instead of assuming it.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_drain_output(ApplicationEngine *engine,
    ApplicationEngineOutput *out_output) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Read the engine's L2 tx count without changing state.
/// @param engine The engine handle.
/// @returns How many L2 transactions the state has executed, which is the history offset of the
/// next one.
/// @details Infallible. Zero at genesis; every successful execution, a counted no-op included,
/// advances it by exactly one with checked arithmetic. It survives dumps.
APPLICATION_ENGINE_API uint64_t application_engine_l2_tx_count(
    const ApplicationEngine *engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Create a crash durable dump of the engine state (write, fsync).
/// @param engine The engine handle.
/// @param prefix The dump to create, must not pre-exist. It carries whatever shape the engine's
/// state does, a directory or a plain file as the engine chooses.
/// @returns OK, NOT_FOUND for a missing required path, IO_ERROR for other filesystem failures,
/// or INTERNAL_ERROR.
/// @details Must be called at a quiescent point only, no in-flight execution. On OK the dump
/// survives an immediate kernel crash, its payload and the directory entry naming it are both
/// synchronized before returning. Failed creation may leave artifacts beneath prefix for the
/// host to remove with the unreferenced checkpoint directory.
///
/// The dump contains the current state. The engine may change backing files or reopen internal
/// handles while checkpointing, but application state and its count stay unchanged. Subsequent
/// execution must leave the completed dump immutable.
/// All checkpoint-owned artifacts reside at or beneath prefix as ordinary files/directories.
/// The host discards a checkpoint by recursive filesystem deletion, with no engine callback.
/// Deletion must leave other checkpoints and independently restored engines usable. Filesystem
/// CoW sharing is allowed when writes remain isolated.
///
/// The file named by application_engine_state_file_in_dump must byte-equal what the canonical
/// machine yields after the same inputs, also for an engine restored from a dump: its whole labeled
/// state range, or its `state` inspect report (docs/protocol/application-contract.md, "Comparison
/// bytes"). The bridge does not verify this cross-build equivalence.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_create_dump(ApplicationEngine *engine,
    const char *prefix) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Get the path of the canonical state file inside a dump.
/// @param prefix The dump to name the state file of.
/// @returns A NUL terminated path, or null on failure with the reason in the last error message.
/// @details Pure over the prefix, it touches no filesystem and needs no engine. Where the state
/// file sits follows from the shape the engine gives a dump, which is why the engine answers
/// rather than a host assuming. An engine whose dump is a directory answers with a file inside
/// it, and one whose dump is the state image itself answers with the prefix unchanged.
/// Its bytes are the comparison bytes that application_engine_create_dump requires.
///
/// The storage is engine owned and thread local, overwritten by the next call on the same
/// thread, so copy rather than retain the pointer. Being fallible, it also clears the last error
/// message like any other fallible call, so read a failure's message before asking this.
APPLICATION_ENGINE_API const char *application_engine_state_file_in_dump(
    const char *prefix) APPLICATION_ENGINE_NOEXCEPT;

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* APPLICATION_ENGINE_H */
