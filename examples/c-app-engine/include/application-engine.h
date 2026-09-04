/* (c) Cartesi and individual authors (see AUTHORS) */
/* SPDX-License-Identifier: Apache-2.0 (see LICENSE) */

#ifndef APPLICATION_ENGINE_H
#define APPLICATION_ENGINE_H

#include <stdint.h>

/// @file
/// The C API of an application engine, and the only surface an engine exports. It mirrors the
/// Cartesi sequencer's Application contract, so the host owning the fold stays application
/// agnostic and an engine is swappable behind this header. Plain C so any host can consume it.
///
/// An application implements these declarations into a static archive, which the `c-app-engine`
/// shim links at build time to become the sequencer's Application, and which the application's
/// own canonical binary links natively from the same objects. One compiled engine on both sides
/// is what makes off-chain and on-chain execution deterministic. An application written in C++,
/// or in any language with a C ABI, implements it the same way.
///
/// Nothing application specific crosses. An engine is handed a dump already holding a configured
/// deployment, so a host never learns what configures the application it runs, and the path is
/// opaque, a file or a directory as the engine chooses.
///
/// This header is the surface a host binds to, and the Rust host generates its declarations from
/// it with bindgen rather than restating them, so a signature changed here cannot disagree with
/// the host that links the engine. The records that cross are plain C layout and the
/// engine must static_assert their sizes and field offsets, so a compiler laying one out
/// differently fails its build rather than the seam. A generated binding carries the same checks
/// on the host side.
///
/// A generated binding follows whatever this header says, so the header carries the compatibility
/// obligation on its own. Every vocabulary here is append only and no value is ever reused, since
/// a host is entitled to refuse a value it does not know rather than to have it renumbered
/// underneath.
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
/// No exception may cross, every entry point catches and reports INTERNAL_ERROR, fatal-no-resume
/// under the declared-death policy, or IO_ERROR when what failed was a filesystem or mapping
/// operation. That split is the point of
/// the second status, an environment out of disk is a condition a caller may act on, an engine
/// that failed its own invariants is not. Only accept or reject is consensus visible, rejection
/// diagnostics are lossy by design.
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

/// @brief The one value this header does not fix, supplied by the application's build.
/// @details The ingress bound on a single user op's method payload is an application sizing
/// decision, the largest payload any of its methods can carry, so it is defined on the compile
/// line rather than here. Every consumer of this header, the engine's own translation units and
/// the binding generation alike, must be given the same value, which is what keeps the bound the
/// host enforces and the bound the engine parses under from being two numbers.
///
/// There is deliberately no default. A silently wrong bound is the exact failure this
/// declaration exists to prevent, so an undefined one stops the build here.
#ifndef APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT
#error "define APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT to the application's largest method payload"
#endif

#ifdef __cplusplus
extern "C" {
#endif

/// @brief The bounds an engine declares, spelled as constants a generated binding can read.
/// @details A binding generator sees a macro only where it is defined, and
/// APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT is defined on the compile line, so the value is
/// restated here as an enumeration constant. That is what carries it across to a host: the
/// application defines one number, and both sides read it from this declaration.
typedef enum ApplicationEngineLimits {
    /// The largest method payload a user op may carry, in bytes. The host publishes it as the
    /// sequencer's MAX_METHOD_PAYLOAD_BYTES and refuses anything larger, so an engine that raised
    /// its own bound without raising this one would never see the payloads it grew to accept.
    APPLICATION_ENGINE_MAX_METHOD_PAYLOAD_BYTES = APPLICATION_ENGINE_METHOD_PAYLOAD_LIMIT,
} ApplicationEngineLimits;

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
    APPLICATION_ENGINE_STATUS_INVALID = -1,        ///< Refused by protocol or configuration validation.
    APPLICATION_ENGINE_STATUS_INTERNAL_ERROR = -2, ///< Internal engine failure, fatal-no-resume.
    APPLICATION_ENGINE_STATUS_IO_ERROR = -3,       ///< A filesystem or mapping operation failed.
} ApplicationEngineStatus;

/// @brief A user op as its sender signed it.
/// @details The engine reads the nonce and the data and carries max_fee without checking it,
/// that guard belongs to the caller, so an op arrives whole.
typedef struct ApplicationEngineUserOp {
    uint32_t nonce;                 ///< Sender replay protection nonce.
    uint16_t max_fee;               ///< Highest frame fee price the sender accepts, in log space.
    ApplicationEngineByteSpan data; ///< Method payload, opaque here and parsed by the engine.
} ApplicationEngineUserOp;

/// @brief A user op that already passed validation, as the caller sequenced it.
/// @details Not the signed op: execution consumes the nonce the state expects, and the max fee
/// went with the guard the caller already settled, leaving the fee the frame charges.
typedef struct ApplicationEngineValidUserOp {
    ApplicationEngineEthereumAddress sender; ///< The recovered signer.
    uint16_t fee;                            ///< The frame fee price charged, in log space.
    ApplicationEngineByteSpan data;          ///< Method payload, opaque here and parsed by the engine.
} ApplicationEngineValidUserOp;

/// @brief An input taken straight from the L1 input box.
/// @details Its sender is authenticated by the chain rather than recovered from a signature,
/// which is what lets the engine trust it without validating anything first.
typedef struct ApplicationEngineDirectInput {
    ApplicationEngineEthereumAddress sender; ///< The L1 authenticated sender.
    uint64_t block_number;                   ///< The L1 inclusion block number.
    ApplicationEngineByteSpan payload;       ///< The raw input payload.
} ApplicationEngineDirectInput;

/// @brief Why an engine refused a user op, mirroring the sequencer's rejection reasons.
/// @details Append only. Written only on APPLICATION_ENGINE_STATUS_INVALID, and it selects which
/// member of ApplicationEngineInvalidValues carries the diagnostics.
typedef enum ApplicationEngineInvalidReason {
    APPLICATION_ENGINE_INVALID_NONCE = 0,            ///< Nonce or account binding, read `nonce`.
    APPLICATION_ENGINE_INVALID_MAX_FEE = 1,          ///< The caller-owned max fee guard, read `max_fee`.
    APPLICATION_ENGINE_INSUFFICIENT_FEE_BALANCE = 2, ///< Cannot cover the frame fee, read `fee_balance`.
} ApplicationEngineInvalidReason;

/// @brief Diagnostics for APPLICATION_ENGINE_INVALID_NONCE.
typedef struct ApplicationEngineInvalidNonce {
    uint32_t expected; ///< The nonce the account expects next.
    uint32_t got;      ///< The nonce the op carried.
} ApplicationEngineInvalidNonce;

/// @brief Diagnostics for APPLICATION_ENGINE_INVALID_MAX_FEE.
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
typedef union ApplicationEngineInvalidValues {
    ApplicationEngineInvalidNonce nonce;                 ///< APPLICATION_ENGINE_INVALID_NONCE.
    ApplicationEngineInvalidMaxFee max_fee;              ///< APPLICATION_ENGINE_INVALID_MAX_FEE.
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
/// @returns OK, IO_ERROR when the filesystem refused, which is what a dump that is not there
/// reports, or INTERNAL_ERROR when one that is there is malformed or holds an invalid deployment.
/// @details The only way to open an engine, and it never creates. A state is written once at
/// genesis by a tool that knows the application's configuration, and every engine afterwards opens
/// what is there, which is what keeps deployment configuration off this API.
///
/// The dump is mapped copy on write, so nothing the engine executes ever reaches a file and the
/// dump stays byte immutable for as long as the engine runs. Two engines may therefore open the
/// same dump, and deleting it under a live engine is safe. In exchange the engine holds no durable
/// state at all, only application_engine_create_dump persists anything. The base file must not be
/// mutated under a live engine, its unwritten pages are still read through.
///
/// The deployment found there is validated before the handle is written, the backstop for a state
/// that was truncated, hand edited, or left by a genesis that died mid-write.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_from_dump(const char *prefix,
    ApplicationEngine **out_engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Destroy an engine instance.
/// @param engine The engine handle.
APPLICATION_ENGINE_API void application_engine_destroy(ApplicationEngine *engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Validate a user op against current state, pure and read-only.
/// @param engine The engine handle.
/// @param sender The recovered signer.
/// @param user_op The op to validate, as its sender signed it.
/// @param current_fee The frame fee price in log space.
/// @param out_invalid Why the op was refused, written whole and only on INVALID.
/// @returns OK, INVALID with diagnostics, or INTERNAL_ERROR.
/// @details A rejection reports itself through out_invalid and leaves the last error message
/// empty, only INTERNAL_ERROR carries one. The max-fee guard belongs to the caller and is never
/// checked here, so APPLICATION_ENGINE_INVALID_MAX_FEE never comes back from this call. Queued
/// outputs are left alone, only an execution touches them.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_validate_user_op(const ApplicationEngine *engine,
    const ApplicationEngineEthereumAddress *sender, const ApplicationEngineUserOp *user_op, uint16_t current_fee,
    ApplicationEngineInvalid *out_invalid) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Execute a validated user op, consuming the current expected nonce.
/// @param engine The engine handle.
/// @param user_op The validated op to execute.
/// @param safe_block The covering frame safe block, folded into the clock as max(clock, it).
/// @param out_output_count How many outputs this op left waiting, written only on OK.
/// @returns OK or INTERNAL_ERROR (an engine throw is fatal-no-resume).
/// @details An op the method rejects still executed and still counts, so it reports OK. Only
/// accept or reject is consensus visible and the state carries it, the seam does not surface
/// the application's own reason.
///
/// An execution refuses to run while an earlier execution's outputs are still queued, reporting
/// INTERNAL_ERROR without executing anything rather than discarding outputs meant to reach the
/// chain. That refusal is what makes the count reported this op's own.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_execute_valid_user_op(ApplicationEngine *engine,
    const ApplicationEngineValidUserOp *user_op, uint64_t safe_block,
    uint64_t *out_output_count) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Execute a direct input from the L1 input box.
/// @param engine The engine handle.
/// @param input The input to execute, its L1 block folded into the clock as max(clock, it).
/// @param out_output_count How many outputs this input left waiting, written only on OK.
/// @returns OK or INTERNAL_ERROR (an engine throw is fatal-no-resume).
/// @details An input the engine rejects is a counted no-op and still reports OK, the same way a
/// rejected user op does. Outputs behave as they do for a user op.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_execute_direct_input(ApplicationEngine *engine,
    const ApplicationEngineDirectInput *input, uint64_t *out_output_count) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Take the next queued output, in emission order.
/// @param engine The engine handle.
/// @param out_output The output taken, written whole and only on OK.
/// @returns OK with an output written, or INTERNAL_ERROR.
/// @details Call it exactly as many times as the execution reported, which is what attributes
/// the outputs to the input that produced them. Taking one more than were queued is a caller bug
/// and reports INTERNAL_ERROR rather than an empty output a host might act on. The payload
/// pointer stays valid until the next drain call releases it, so copy before draining again.
/// A voucher carries its value even when an engine only ever emits zero-value ones, so a host
/// reads what the engine emitted instead of assuming it.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_drain_output(ApplicationEngine *engine,
    ApplicationEngineOutput *out_output) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Get the maximum block carried by any executed input (the engine's safe-block clock).
/// @param engine The engine handle.
/// @returns The last executed safe block, zero when nothing has executed.
/// @details Carried by execution rather than set, so an engine cannot execute and forget to
/// advance it. It lives in the state, so a resumed one reports the block it reflects.
APPLICATION_ENGINE_API uint64_t application_engine_last_executed_safe_block(
    const ApplicationEngine *engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Get the count of executed inputs, user ops and direct inputs alike.
/// @param engine The engine handle.
/// @returns The executed input count.
APPLICATION_ENGINE_API uint64_t application_engine_executed_input_count(
    const ApplicationEngine *engine) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Create a crash durable dump of the engine state (write, fsync).
/// @param engine The engine handle.
/// @param prefix The dump to create, must not pre-exist. It carries whatever shape the engine's
/// state does, a directory or a plain file as the engine chooses.
/// @returns OK, IO_ERROR when the filesystem refused, which is what a full filesystem or an
/// exhausted quota reports, or INTERNAL_ERROR.
/// @details Must be called at a quiescent point only, no in-flight execution. On OK the dump
/// survives an immediate kernel crash, its payload and the directory entry naming it are both
/// synchronized before returning. An engine that cleans up after a failed write leaves the prefix
/// free for a clean retry, which a host cannot do on its behalf.
///
/// The dump is the live image, not a copy of the file the engine was opened over. The write is
/// sparse, so a dump costs what the image populates rather than its whole length, while finding
/// that reads the whole length either way.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_create_dump(const ApplicationEngine *engine,
    const char *prefix) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Delete a previously created dump.
/// @param prefix The dump to remove.
/// @returns OK, IO_ERROR when the filesystem refused or the dump was not there, or
/// INTERNAL_ERROR.
/// @details An engine still holding this dump open keeps running, its mapping outlives the name.
/// Synchronizing the directory entry before returning is what would let a caller drop its record
/// of the path on OK, so an engine that skips it can leave an orphan behind a crash.
APPLICATION_ENGINE_API ApplicationEngineStatus application_engine_delete_dump(
    const char *prefix) APPLICATION_ENGINE_NOEXCEPT;

/// @brief Get the path of the canonical state file inside a dump.
/// @param prefix The dump to name the state file of.
/// @returns A NUL terminated path, or null on failure with the reason in the last error message.
/// @details Pure over the prefix, it touches no filesystem and needs no engine. Where the state
/// file sits follows from the shape the engine gives a dump, which is why the engine answers
/// rather than a host assuming. An engine whose dump is a directory answers with a file inside
/// it, and one whose dump is the state image itself answers with the prefix unchanged.
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
