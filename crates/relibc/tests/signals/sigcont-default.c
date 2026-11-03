#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "../test_helpers.h"

/*
 * A SIGCONT sent to a running process whose disposition is the default must
 * not interrupt a blocking call: its default action only continues a stopped
 * process, and no handler runs. Shells send SIGCONT to the process group of
 * every job they start, so a program blocked in a read when that arrives would
 * otherwise see EINTR (issue 377).
 *
 * The child blocks reading a pipe; the parent sends it SIGCONT, waits, then
 * writes one byte. The read must return that byte.
 */

#define ROUNDS 8

static void pause_100ms(void)
{
	struct timespec delay = { .tv_sec = 0, .tv_nsec = 100000000 };
	int status = nanosleep(&delay, NULL);
	ERROR_IF(nanosleep, status, == -1);
}

static void child(int read_fd)
{
	char byte = 0;
	ssize_t got = read(read_fd, &byte, 1);
	if (got != 1 || byte != 'x')
	{
		printf("child: read returned %zd (%s), want 1 byte\n",
		       got, got < 0 ? strerror(errno) : "no error");
		_exit(EXIT_FAILURE);
	}
	_exit(EXIT_SUCCESS);
}

static void round_trip(int round)
{
	int fds[2];
	int status = pipe(fds);
	ERROR_IF(pipe, status, == -1);

	pid_t pid = fork();
	ERROR_IF(fork, pid, == -1);
	if (pid == 0)
	{
		close(fds[1]);
		child(fds[0]);
	}
	close(fds[0]);

	/* Let the child block in its read before it is signalled. */
	pause_100ms();
	status = kill(pid, SIGCONT);
	ERROR_IF(kill, status, == -1);
	pause_100ms();

	ssize_t written = write(fds[1], "x", 1);
	ERROR_IF(write, written, != 1);
	close(fds[1]);

	pid_t waited = waitpid(pid, &status, 0);
	ERROR_IF(waitpid, waited, == -1);
	if (!WIFEXITED(status) || WEXITSTATUS(status) != EXIT_SUCCESS)
	{
		printf("round %d: the child's read was interrupted by SIGCONT\n", round);
		exit(EXIT_FAILURE);
	}
}

int main(void)
{
	for (int round = 0; round < ROUNDS; round++)
	{
		round_trip(round);
	}
	puts("SIGCONT with the default action did not interrupt a blocked read");
	return EXIT_SUCCESS;
}
