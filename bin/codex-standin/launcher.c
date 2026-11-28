// The `codex` a stand-in pane runs.  Compiled so that the process in
// front of the pane is what marspot looks for in a codex: argv[0]
// `codex`, and an executable wherever the test puts it (a release
// directory of the packaged layout).  The work is done by standin.py in
// a child; this process passes on the signals a terminal or marspot
// sends and leaves with the child's status.
//
//   cc -DPYTHON='"/path/python3"' -DSTANDIN='"/path/standin.py"' -o codex launcher.c
//
// The kimi stand-in is built from it too, with -DNAME='"kimi"'.
#include <libproc.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef NAME
#define NAME "codex"
#endif

static pid_t child;

static void pass(int sig) {
    if (child > 0) kill(child, sig);
}

int main(int argc, char **argv) {
    char exe[PROC_PIDPATHINFO_MAXSIZE] = "";
    proc_pidpath(getpid(), exe, sizeof exe);
    setenv("CODEX_STANDIN_EXE", exe, 1);
    char **args = calloc((size_t)argc + 3, sizeof *args);
    if (!args) return 70;
    args[0] = PYTHON;
    args[1] = "-I";
    args[2] = STANDIN;
    for (int i = 1; i < argc; i++) args[i + 2] = argv[i];
    child = fork();
    if (child < 0) return 71;
    if (child == 0) {
        execv(PYTHON, args);
        perror(NAME " stand-in: " PYTHON);
        _exit(127);
    }
    int sigs[] = {SIGTERM, SIGHUP, SIGINT, SIGQUIT, SIGWINCH};
    for (size_t i = 0; i < sizeof sigs / sizeof *sigs; i++) signal(sigs[i], pass);
    int status;
    while (waitpid(child, &status, 0) < 0) {
    }
    if (WIFSIGNALED(status)) return 128 + WTERMSIG(status);
    return WEXITSTATUS(status);
}
