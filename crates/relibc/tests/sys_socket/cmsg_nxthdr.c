#include <sys/socket.h>

/* No system calls: this also runs on the host against freshly generated headers. */
#define CHECK(condition) do { if (!(condition)) return __LINE__; } while (0)

int main(void) {
    union {
        struct cmsghdr alignment;
        unsigned char bytes[CMSG_SPACE(1) + CMSG_SPACE(0)];
    } control = {0};
    struct msghdr msg = {0};
    msg.msg_control = control.bytes;

    msg.msg_controllen = sizeof(struct cmsghdr) - 1;
    CHECK(CMSG_FIRSTHDR(&msg) == 0);
    msg.msg_controllen = sizeof(control.bytes);
    struct cmsghdr *first = CMSG_FIRSTHDR(&msg);
    CHECK(first == (struct cmsghdr *)control.bytes);

    first->cmsg_len = sizeof(struct cmsghdr) - 1;
    CHECK(CMSG_NXTHDR(&msg, first) == 0);
    first->cmsg_len = CMSG_LEN(1);
    struct cmsghdr *second = (struct cmsghdr *)(control.bytes + CMSG_SPACE(1));
    second->cmsg_len = CMSG_LEN(0);

    /* An aligned header fits exactly, even with no payload in the last message. */
    CHECK(CMSG_NXTHDR(&msg, first) == second);
    CHECK(CMSG_NXTHDR(&msg, second) == 0);
    msg.msg_controllen--;
    CHECK(CMSG_NXTHDR(&msg, first) == 0);
    msg.msg_controllen = CMSG_SPACE(1);
    CHECK(CMSG_NXTHDR(&msg, first) == 0);

    msg.msg_controllen = sizeof(control.bytes);
    first->cmsg_len = sizeof(control.bytes);
    CHECK(CMSG_NXTHDR(&msg, first) == 0);
    return 0;
}
