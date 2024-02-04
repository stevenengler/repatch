# Publishing a new release

1. Update the code.

    ```bash
    # make sure we don't include personal information (such as our
    # home directory name) in the release
    cd /tmp

    # make sure we don't include any untracked files in the release
    git clone git@github.com:stevenengler/repatch.git
    cd repatch

    # update the version
    vim Cargo.toml
    cargo update --package repatch

    # check for errors
    git diff
    cargo publish --dry-run --allow-dirty

    # add and commit version changes with commit message, for example
    # "Updated version to '0.2.1'"
    git add --patch
    git commit
    git push
    ```

2. After CI tests finish on GitHub, mark it as a new release.

3. Publish the crate.

    ```bash
    # make sure there are no untracked or changed files
    git status

    # publish
    cargo publish --dry-run
    cargo publish
    ```
