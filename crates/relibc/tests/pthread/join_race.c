// Ferric Oxide: joins many short-lived threads. A thread posts its return value and then wakes
// the joiner; the joiner can see the value first, return and free the thread's control block
// before that wake runs, so the wake must tolerate the block being gone (EFAULT).

#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#include "common.h"

#define ROUNDS 2000

static void *routine(void *arg) {
    return arg;
}

int main(void) {
    for (uintptr_t round = 0; round < ROUNDS; round++) {
        pthread_t thread;
        const pthread_attr_t *attr = NULL;
        void *arg = (void *)round;
        int status = pthread_create(&thread, attr, routine, arg);
        if (status != 0) {
            return fail(status, "pthread_create");
        }
        void *retval;
        status = pthread_join(thread, &retval);
        if (status != 0) {
            return fail(status, "pthread_join");
        }
        if ((uintptr_t)retval != round) {
            fprintf(stderr, "round %lu: thread returned %p\n", (unsigned long)round, retval);
            return EXIT_FAILURE;
        }
    }
    puts("joined all threads");
    return EXIT_SUCCESS;
}
