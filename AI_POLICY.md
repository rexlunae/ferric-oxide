# AI Contribution Policy

Ferric Oxide welcomes contributions made with the help of AI tools, including large language models, coding assistants and autonomous coding agents. We judge a contribution by whether it is correct, tested, maintainable and legally sound, not by which tools produced it.

This is a deliberate difference from upstream Redox OS, which does not accept LLM-generated contributions.

## What we ask of every contribution

These rules apply whether a change was typed by hand, generated, or both.

1. **A human is accountable.** Every pull request has a human author who has read the whole change, understands it, and will respond to review. "The AI wrote it" is never an answer to a review question.
2. **It meets the normal bar.** AI-assisted changes go through the same review, CI and testing as any other change. Kernel, driver, file system and memory-safety code (anything with `unsafe`) gets the same extra scrutiny it always does.
3. **It is tested.** Describe how you verified the change: the commands you ran, the targets you booted, the benchmarks you measured. A change that has only been generated, never built or run, is not ready for review.
4. **It is licensed correctly.** You certify the contribution under the [Developer Certificate of Origin](https://developercertificate.org/). You are responsible for making sure AI output does not reproduce code under a license incompatible with MIT. If a tool produces a large verbatim block that looks like it came from somewhere else, find the source or rewrite it.
5. **It is sized for review.** Keep pull requests focused. Generating code is cheap; reviewing it is not. Split large changes into reviewable steps.

## Disclosure

Disclosure is encouraged and never penalized. If AI tools did a meaningful share of the work, say so in the pull request description, and if you like, add a trailer to the commit:

```
Assisted-by: <tool or model name>
```

Disclosure helps reviewers know where to look carefully. It does not lower or raise the bar for acceptance.

## AI agents

Autonomous agents may open issues and pull requests on behalf of a human sponsor. The sponsor is named in the pull request, is the accountable author under rule 1, and handles anything the agent cannot. Agents must follow the same rules as people: no bulk or low-effort submissions, no repeated pings, and no pull requests opened just to farm activity.

## What is not acceptable

- Submissions nobody has read, built or tested.
- Mass-generated pull requests, issues or comments.
- Using AI to fabricate test results, benchmarks, hardware reports or citations.
- Sending secrets, private data or embargoed security details to third-party AI services.

Maintainers may close contributions that break these rules, whatever tools were used.

## Contributing back to Redox OS

Upstream Redox OS has its own policy, and it rejects LLM-generated contributions. Please respect it. Do not submit AI-generated work from this project to Redox OS repositories. Fixes you wrote yourself are welcome to go upstream under Redox's own contribution rules.
