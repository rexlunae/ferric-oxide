# Creating Proper Pull Requests

Changes reach Ferric Oxide through GitHub pull requests, which are reviewed and
merged into `main`.

For small changes, open a pull request directly. For larger ones, such as a
change to a kernel interface, a scheme, the file system format or the build
system layout, start with an issue labelled `proposal` so the approach can be
agreed before you write the code (see [Filing Issues](./filing-issues.md)).

## Preparing your branch

1. Fork [rexlunae/ferric-oxide](https://github.com/rexlunae/ferric-oxide) on
   GitHub, then clone your fork and add the main repository as `upstream`:

    ```sh
    git clone https://github.com/YOUR_USERNAME/ferric-oxide.git
    cd ferric-oxide
    git remote add upstream https://github.com/rexlunae/ferric-oxide.git
    ```

2. Set up the build, if you haven't already (see [Build and Configuration](./ferric-build.md)):

    ```sh
    cargo xtask setup
    ```

3. Start a branch from the latest `main`:

    ```sh
    git fetch upstream main
    git checkout -b MY_BRANCH upstream/main
    ```

4. Make your changes, and update the documentation they affect in the same
   branch (see [Writing Documentation Correctly](./writing-docs-correctly.md)).

5. Check and test them. At least:

    ```sh
    cargo fmt --all --check
    cargo test --locked --workspace
    cargo xtask build
    cargo xtask run --headless
    ```

   Changes to the kernel, drivers or boot path need an image boot. Changes
   that claim a speedup need before-and-after numbers. [Validation and
   Debugging](./ferric-testing.md) lists the checks CI runs and how to run
   them locally.

6. Commit with a sign-off, which certifies your contribution under the
   [Developer Certificate of Origin](https://developercertificate.org/):

    ```sh
    git commit -s
    ```

   Write commit messages in the imperative ("Add stuff to file.ext", not
   "added stuff to file.ext"), with a body that explains why.

7. Rebase onto the latest `main` before you push, rather than merging `main`
   into your branch:

    ```sh
    git fetch upstream main
    git rebase upstream/main
    git push -u origin MY_BRANCH
    ```

## Submitting the pull request

Open a pull request from your branch against `main` and fill in the template:

- **Problem:** what the change solves, with a link to the issue if there is one.
- **Solution:** what you changed and why this way.
- **Testing:** the commands you ran, the targets you built and booted, and
  benchmark numbers for performance work.
- **AI assistance:** which tools did a meaningful share of the work, if any.
  See the [AI Contribution Policy](https://github.com/rexlunae/ferric-oxide/blob/main/AI_POLICY.md).

Keep each pull request to one change. Open it as a draft while it is still in
progress, and mark it ready for review when CI is green and you've finished.
GitHub notifies reviewers; there's nothing else to post.

## Incorporating feedback

When a reviewer asks for changes, push new commits to the same branch and
reply to each comment, either with the fix or with why you disagree. If you
need to make large changes after review has started, mark the pull request as
a draft again until they are done, so reviewers don't spend time on code that
is still moving.

If your change is also useful to Redox OS and you wrote it entirely yourself,
you may send it to Redox under their rules. Never send AI-assisted work there:
Redox does not accept it.
