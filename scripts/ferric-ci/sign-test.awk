# Sign-test confidence bounds on the median of paired differences, for a gate that may look
# at the same growing sample several times (check-trace-overhead.sh; image.yml's
# trace-overhead job adds pairs while a comparison is inconclusive).
#
# Input: the paired differences, one per line, in any order.
# Variables:
#   alpha  the overall one-sided error rate (default 0.05): the chance, when the true median
#          is exactly at a value, that any planned look puts its lower bound above it;
#   looks  the pair counts at which the gate may judge, space-separated and ascending
#          (default: only the number of input lines, a single look);
#   mode   "bounds" (default) prints "k low high" for the input: the k-th smallest and
#          k-th largest differences, or "0 none none" when no bound reaches the level;
#          "k" prints "n k" for every planned look, without reading a bound from input;
#          "rate" prints the per-look level and the exact overall error with it, then the
#          overall error if every look used alpha itself.
#
# With n pairs, the k-th smallest difference is a one-sided lower bound on the median with
# error P(Binomial(n, 1/2) <= k - 1), assuming only independent pairs (no normality or
# symmetry). Looking again at a larger sample that contains the first raises the overall
# error above any one look's, so every look uses the same per-look level a, the largest
# for which the exact chance of a false lower bound at some look is at most alpha. That
# chance is computed by dynamic programming over the number of pairs at or below the true
# median, which under the null is a fair binomial walk; a look rejects when fewer than its
# k pairs are at or below. Looks that stop early because the median passed only lower it,
# so the level is conservative for the job's stopping rule. With a single look a is alpha.
# Exits 2 when the input's pair count is not one of the looks.

# The binomial terms are summed in log space so 2^n and the coefficients cannot overflow.
function binomial_cdf(n, cdf,    i, logp, sum) {
    logp = -n * log(2)
    sum = 0
    for (i = 0; i <= n; i++) {
        sum += exp(logp)
        cdf[i] = sum
        if (i < n) logp += log(n - i) - log(i + 1)
    }
}

# The largest k with P(Binomial(n, 1/2) <= k - 1) <= level, from the CDF of look j.
function k_at(j, level,    k) {
    k = 0
    while (k < look[j] && cdfs[j, k] <= level) k++
    return k
}

# The chance under the null that some look's lower bound, at per-look level `level`,
# excludes the true median.
function overall(level,    j, n, s, p, q, reject, kj) {
    split("", p)
    p[0] = 1
    n = 0
    reject = 0
    for (j = 1; j <= nlooks; j++) {
        for (; n < look[j]; n++) {
            split("", q)
            for (s = 0; s <= n; s++) {
                q[s] += p[s] / 2
                q[s + 1] += p[s] / 2
            }
            split("", p)
            for (s = 0; s <= n + 1; s++) p[s] = q[s]
        }
        kj = k_at(j, level)
        for (s = 0; s < kj; s++) {
            reject += p[s]
            p[s] = 0
        }
    }
    return reject
}

function plan(    j, i, lo, hi, mid, c) {
    if (alpha == "") alpha = 0.05
    if (looks == "") looks = NR
    nlooks = split(looks, look, " ")
    for (j = 1; j <= nlooks; j++) {
        look[j] += 0
        if (j > 1 && look[j] <= look[j - 1]) {
            print "sign-test.awk: looks must ascend: " looks > "/dev/stderr"
            exit 2
        }
        split("", c)
        binomial_cdf(look[j], c)
        for (i = 0; i <= look[j]; i++) cdfs[j, i] = c[i]
    }
    # overall() is a non-decreasing step function of the level, and 0 at level 0.
    lo = 0
    hi = alpha
    if (overall(hi) <= alpha) lo = hi
    else for (i = 0; i < 60; i++) {
        mid = (lo + hi) / 2
        if (overall(mid) <= alpha) lo = mid; else hi = mid
    }
    level = lo
}

{ v[NR] = $1 + 0 }

END {
    plan()
    if (mode == "rate") {
        printf "%.6f %.6f %.6f\n", level, overall(level), overall(alpha)
        exit 0
    }
    if (mode == "k") {
        for (j = 1; j <= nlooks; j++) print look[j], k_at(j, level)
        exit 0
    }
    current = 0
    for (j = 1; j <= nlooks; j++) if (look[j] == NR) current = j
    if (!current) {
        print "sign-test.awk: " NR " pairs is not one of the planned looks (" looks ")" > "/dev/stderr"
        exit 2
    }
    k = k_at(current, level)
    if (k == 0) {
        print "0 none none"
        exit 0
    }
    asort_numeric(v, NR)
    printf "%d %.9f %.9f\n", k, v[k], v[NR + 1 - k]
}

# Sorts a[1..n] in place (insertion sort: n is a few dozen pairs). POSIX awk has no asort.
function asort_numeric(a, n,    i, j, t) {
    for (i = 2; i <= n; i++) {
        t = a[i]
        for (j = i - 1; j >= 1 && a[j] > t; j--) a[j + 1] = a[j]
        a[j + 1] = t
    }
}
