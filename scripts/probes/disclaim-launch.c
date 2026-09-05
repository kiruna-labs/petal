// Disclaiming launcher (#747 dev rig). Spawns a target binary with
// responsibility DISCLAIMED (private libsystem API, the mechanism Chromium et
// al. use), so the child is its OWN responsible process for TCC. Result: the
// child's AXIsProcessTrustedWithOptions prompt registers the CHILD binary in
// the Accessibility pane -- not Terminal -- which is the only way a bare
// (non-bundle) binary can obtain the DIRECT Accessibility grant that Darwin
// 25.2 requires for real AX window elements (inherited trust returns redacted
// app-element copies; see WINDOW_REGISTRY_PLAN.md §9.13/§9.14).
//
//   clang -o disclaim-launch disclaim-launch.c
//   ./disclaim-launch /path/to/binary [args...]
#include <dlfcn.h>
#include <spawn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

extern char **environ;

typedef int (*disclaim_fn)(posix_spawnattr_t *, int);

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr, "usage: %s <binary> [args...]\n", argv[0]);
        return 2;
    }
    disclaim_fn disclaim =
        (disclaim_fn)dlsym(RTLD_DEFAULT, "responsibility_spawnattrs_setdisclaim");
    if (!disclaim) {
        fprintf(stderr, "responsibility_spawnattrs_setdisclaim not found\n");
        return 3;
    }
    posix_spawnattr_t attr;
    posix_spawnattr_init(&attr);
    int rc = disclaim(&attr, 1);
    fprintf(stderr, "disclaim rc=%d; spawning %s\n", rc, argv[1]);
    pid_t pid = 0;
    rc = posix_spawn(&pid, argv[1], NULL, &attr, &argv[1], environ);
    posix_spawnattr_destroy(&attr);
    if (rc != 0) {
        fprintf(stderr, "posix_spawn failed: %s\n", strerror(rc));
        return 4;
    }
    fprintf(stderr, "spawned pid=%d (disclaimed)\n", pid);
    int status = 0;
    waitpid(pid, &status, 0);
    return WIFEXITED(status) ? WEXITSTATUS(status) : 128;
}
