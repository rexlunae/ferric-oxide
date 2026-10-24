/* Interrupt a memcpy-shaped register state without making a C call in the
 * interrupted region. A call to raise() would permit x1/x2 to be clobbered and
 * cannot test asynchronous signal return. */
#include <assert.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#if defined(__redox__)
/* The generated pthread.h and unistd.h currently disagree on this macro. */
#undef _POSIX_THREADS
#endif
#include <unistd.h>

#if defined(__aarch64__)
enum { ROUNDS = 64, COPY_SIZE = 64 };
#if defined(__redox__)
enum { TOTAL_ROUNDS = 2 * ROUNDS };
#else
enum { TOTAL_ROUNDS = ROUNDS };
#endif
_Static_assert(__atomic_always_lock_free(sizeof(unsigned), 0),
               "signal handshake must be lock-free");

static unsigned ready;
static unsigned handled;
static pid_t receiver;
static const char source[COPY_SIZE] = "signal return must preserve source and length";
static const uint64_t vectors[4] = {
    UINT64_C(0x0123456789abcdef), UINT64_C(0xfedcba9876543210),
    UINT64_C(0x1122334455667788), UINT64_C(0x8877665544332211)
};

static void timeout(int signo) {
    (void)signo;
    _exit(124);
}

static void handler(int signo) {
    assert(signo == SIGUSR1);
    /* These are caller-saved at a C call, but must survive an interruption. */
    __asm__ volatile (
        "movi v0.16b, #0\n"
        "movi v1.16b, #0\n"
        "msr fpcr, xzr\n"
        "msr fpsr, xzr\n"
        : : : "v0", "v1", "memory"
    );
    __atomic_store_n(&handled, __atomic_load_n(&ready, __ATOMIC_RELAXED),
                     __ATOMIC_RELEASE);
}

static void *sender(void *unused) {
    (void)unused;
    for (unsigned round = 1; round <= TOTAL_ROUNDS; ++round) {
        while (__atomic_load_n(&ready, __ATOMIC_ACQUIRE) < round) {
            sched_yield();
        }
        assert(kill(receiver, SIGUSR1) == 0);
        while (__atomic_load_n(&handled, __ATOMIC_ACQUIRE) < round) {
            sched_yield();
        }
    }
    return NULL;
}

#if defined(__redox__)
/* Seed the argument register with an error-shaped value. A signal selected
 * inside yield must resume with the completed syscall's zero result instead.
 * Calling libc avoids duplicating the kernel's syscall-number definition. */
static int yield_probe(void) {
    register long result __asm__("x0") = -1;
    __asm__ volatile (
        "bl sched_yield\n"
        : "+r" (result)
        :
        : "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9",
          "x10", "x11", "x12", "x13", "x14", "x15", "x16", "x17",
          "x18", "x30", "v0", "v1", "v2", "v3", "v4", "v5", "v6", "v7",
          "v8", "v9", "v10", "v11", "v12", "v13", "v14", "v15",
          "v16", "v17", "v18", "v19", "v20", "v21", "v22", "v23",
          "v24", "v25", "v26", "v27", "v28", "v29", "v30", "v31",
          "cc", "memory"
    );
    return (int)result;
}
#endif
#endif

int main(void) {
#if defined(__aarch64__)
    struct sigaction action = { .sa_handler = handler };
    sigset_t blocked;
    pthread_t thread;
    receiver = getpid();
    assert(sigemptyset(&action.sa_mask) == 0);
    assert(sigaction(SIGUSR1, &action, NULL) == 0);
    assert(signal(SIGALRM, timeout) != SIG_ERR);
    alarm(30);

    /* The sender inherits the blocked mask, so process-directed signals can
     * only interrupt the receiver after its assembly has published ready. */
    assert(sigemptyset(&blocked) == 0);
    assert(sigaddset(&blocked, SIGUSR1) == 0);
    assert(pthread_sigmask(SIG_BLOCK, &blocked, NULL) == 0);
    assert(pthread_create(&thread, NULL, sender, NULL) == 0);
    assert(pthread_sigmask(SIG_UNBLOCK, &blocked, NULL) == 0);

    for (unsigned round = 1; round <= ROUNDS; ++round) {
        uintptr_t observed[2];
        uint64_t observed_vectors[4], observed_control[2], old_control[2];
        __asm__ volatile (
            "mrs %0, fpcr\n"
            "mrs %1, fpsr\n"
            : "=r" (old_control[0]), "=r" (old_control[1])
        );
        __asm__ volatile (
            "mov x1, %[source]\n"
            "mov x2, #64\n"
            "ldp q0, q1, [%[vectors]]\n"
            "mov x9, #0x400000\n"
            "msr fpcr, x9\n"
            "mov x9, #0x10\n"
            "msr fpsr, x9\n"
            "stlr %w[round], [%[ready]]\n"
            "1: ldar w9, [%[handled]]\n"
            "cmp w9, %w[round]\n"
            "b.lo 1b\n"
            "stp x1, x2, [%[observed]]\n"
            "stp q0, q1, [%[observed_vectors]]\n"
            "mrs x9, fpcr\n"
            "str x9, [%[observed_control]]\n"
            "mrs x9, fpsr\n"
            "str x9, [%[observed_control], #8]\n"
            :
            : [source] "r" (source), [round] "r" (round),
              [ready] "r" (&ready), [handled] "r" (&handled),
              [observed] "r" (observed), [vectors] "r" (vectors),
              [observed_vectors] "r" (observed_vectors),
              [observed_control] "r" (observed_control)
            : "x1", "x2", "x9", "v0", "v1", "cc", "memory"
        );
        __asm__ volatile (
            "msr fpcr, %0\n"
            "msr fpsr, %1\n"
            : : "r" (old_control[0]), "r" (old_control[1])
        );
        if (observed[0] != (uintptr_t)source || observed[1] != COPY_SIZE) {
            fprintf(stderr, "signal-registers: round %u: x1=%#lx x2=%#lx\n",
                    round, (unsigned long)observed[0], (unsigned long)observed[1]);
            return 1;
        }
        if (memcmp(observed_vectors, vectors, sizeof(vectors)) != 0 ||
            observed_control[0] != UINT64_C(0x400000) ||
            observed_control[1] != UINT64_C(0x10)) {
            fprintf(stderr, "signal-registers: round %u: SIMD/control state changed\n", round);
            return 1;
        }
        char copy[COPY_SIZE];
        memcpy(copy, (const void *)observed[0], observed[1]);
        assert(memcmp(copy, source, sizeof(copy)) == 0);
    }
#if defined(__redox__)
    for (unsigned round = ROUNDS + 1; round <= TOTAL_ROUNDS; ++round) {
        __atomic_store_n(&ready, round, __ATOMIC_RELEASE);
        do {
            if (yield_probe() != 0) {
                fprintf(stderr, "signal-registers: round %u: syscall result changed\n", round);
                return 1;
            }
        } while (__atomic_load_n(&handled, __ATOMIC_ACQUIRE) < round);
    }
    puts("FERRIC_TEST_PASS signal-syscall-result");
#endif
    assert(pthread_join(thread, NULL) == 0);
    alarm(0);
    puts("FERRIC_TEST_PASS signal-registers");
#else
    puts("FERRIC_TEST_SKIP signal-registers: requires ARM64");
#endif
    return 0;
}
