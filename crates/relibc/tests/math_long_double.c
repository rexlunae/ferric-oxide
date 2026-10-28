#include <math.h>
#include <stdio.h>

// Long double functions come from openlibm's ld80/ld128 sources, which are
// only built when openlibm knows long double is wider than double on the
// target (it is on x86-64 and ARM64 Redox). Linking this test catches a libm
// built without them.
int main(void) {
    // volatile keeps the compiler from folding the calls away.
    volatile long double eight = 8.0L, four = 4.0L, seven = 7.0L;
    int exp = 0;
    long double mantissa = frexpl(eight, &exp);
    if (mantissa != 0.5L || exp != 4) {
        printf("frexpl(8) = %Lf * 2^%d\n", mantissa, exp);
        return 1;
    }
    if (sqrtl(four) != 2.0L) {
        printf("sqrtl(4) = %Lf\n", sqrtl(four));
        return 1;
    }
    if (fmodl(seven, four) != 3.0L) {
        printf("fmodl(7, 4) = %Lf\n", fmodl(seven, four));
        return 1;
    }
    return 0;
}
