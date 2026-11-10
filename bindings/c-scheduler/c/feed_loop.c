/* (c) Cartesi and individual authors (see AUTHORS) */
/* SPDX-License-Identifier: Apache-2.0 (see LICENSE) */

/* The canonical feed/next loop over scheduler.h, without an application: it prints what the
 * scheduler hands out instead of executing it. tests/c_example.rs builds it against
 * libc_scheduler.a and compares its output with the Rust reference on the same inputs. */

#include <inttypes.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>

#include "scheduler.h"

/* The header's enums cross as 32-bit integers, which a C compiler need not pick on its own. */
_Static_assert(sizeof(SchedulerStatus) == sizeof(int32_t), "SchedulerStatus is not 32 bits");
_Static_assert(sizeof(SchedulerTxKind) == sizeof(int32_t), "SchedulerTxKind is not 32 bits");
_Static_assert(sizeof(SchedulerInputOutcomeKind) == sizeof(int32_t),
    "SchedulerInputOutcomeKind is not 32 bits");
_Static_assert(sizeof(SchedulerBatchRejectReason) == sizeof(int32_t),
    "SchedulerBatchRejectReason is not 32 bits");

#define SEQUENCER_BYTE 0x11
#define USER_BYTE 0x22
#define CHAIN_ID 1

/* ssz(Batch { nonce: 0, frames: [Frame { user_ops: [], safe_block: 2, fee_price: 0 }] }) */
static const uint8_t BATCH[] = {
    0, 0, 0, 0, 0, 0, 0, 0, /* nonce */
    12, 0, 0, 0,            /* offset of frames */
    4, 0, 0, 0,             /* offset of frame 0 within the list */
    14, 0, 0, 0,            /* offset of user_ops within the frame */
    2, 0, 0, 0, 0, 0, 0, 0, /* safe_block */
    0, 0,                   /* fee_price */
};

static SchedulerInput input_at(uint64_t index, uint8_t sender, uint64_t block, const uint8_t *payload,
    uint64_t size) {
    SchedulerInput input;
    memset(&input, 0, sizeof input);
    input.index = index;
    memset(input.sender.bytes, sender, sizeof input.sender.bytes);
    input.block_number = block;
    input.timestamp = block * 12;
    input.chain_id = CHAIN_ID;
    input.payload.data = payload;
    input.payload.size = size;
    return input;
}

static int fail(const char *what, SchedulerStatus status) {
    fprintf(stderr, "%s failed with %d: %s\n", what, (int)status, scheduler_get_last_error_message());
    return 1;
}

int main(void) {
    if (scheduler_abi_version() != SCHEDULER_ABI_VERSION) {
        fprintf(stderr, "scheduler ABI version %" PRIu32 ", expected %d\n", scheduler_abi_version(),
            SCHEDULER_ABI_VERSION);
        return 1;
    }

    SchedulerConfig config;
    memset(&config, 0, sizeof config);
    memset(config.sequencer_address.bytes, SEQUENCER_BYTE, sizeof config.sequencer_address.bytes);
    config.max_wait_blocks = 10;
    config.fridge_bound = 16;
    config.max_direct_payload_bytes = 4;

    Scheduler *scheduler = NULL;
    SchedulerStatus status = scheduler_create(&config, &scheduler);
    if (status != SCHEDULER_STATUS_OK) {
        return fail("scheduler_create", status);
    }

    static const uint8_t A[] = {'a'}, B[] = {'b'}, C[] = {'c'}, LONG[] = {'l', 'o', 'n', 'g', '!'};
    const SchedulerInput inputs[] = {
        input_at(0, USER_BYTE, 1, A, sizeof A),
        input_at(1, USER_BYTE, 2, B, sizeof B),
        input_at(2, USER_BYTE, 3, C, sizeof C),
        input_at(3, USER_BYTE, 4, LONG, sizeof LONG), /* over the payload cap: dropped */
        input_at(4, SEQUENCER_BYTE, 5, BATCH, sizeof BATCH),
        input_at(5, USER_BYTE, 20, NULL, 0), /* the backstop drains the direct at block 3 */
    };

    for (size_t i = 0; i < sizeof inputs / sizeof inputs[0]; i++) {
        SchedulerInputOutcome outcome;
        status = scheduler_feed(scheduler, &inputs[i], &outcome);
        if (status != SCHEDULER_STATUS_OK) {
            scheduler_destroy(scheduler);
            return fail("scheduler_feed", status);
        }
        printf("input %" PRIu64 " outcome %d\n", inputs[i].index, (int)outcome.kind);

        SchedulerTx tx;
        while ((status = scheduler_next_tx(scheduler, &tx)) == SCHEDULER_STATUS_OK) {
            /* A guest would execute here: a direct always, a user op at its sender's next nonce. */
            if (tx.kind == SCHEDULER_TX_DIRECT) {
                printf("direct %" PRIu64 " block %" PRIu64 " bytes %" PRIu64 " batch %" PRIu64 "\n",
                    tx.values.direct.input_index, tx.values.direct.block_number,
                    tx.values.direct.payload.size, tx.batch_nonce);
            } else {
                printf("user_op nonce %" PRIu32 " block %" PRIu64 " batch %" PRIu64 "\n",
                    tx.values.user_op.nonce, tx.values.user_op.safe_block, tx.batch_nonce);
            }
        }
        if (status != SCHEDULER_STATUS_DONE) {
            scheduler_destroy(scheduler);
            return fail("scheduler_next_tx", status);
        }
    }

    SchedulerState state;
    status = scheduler_state(scheduler, &state);
    if (status != SCHEDULER_STATUS_OK) {
        scheduler_destroy(scheduler);
        return fail("scheduler_state", status);
    }
    printf("state %" PRIu64 " %" PRIu64 " %" PRIu64 " %" PRIu64 " %" PRIu64 "\n", state.l1_input_count,
        state.next_batch_nonce, state.clock, state.first_queued_input, state.fridge_len);
    scheduler_destroy(scheduler);
    return 0;
}
