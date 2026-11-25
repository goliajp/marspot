// Stand-ins for bin/test-zombie-watch.sh, one binary under four names:
//
//   claude  a claude session: a session record with an inbox socket,
//           whatever arrives appended to $FAKE_DIR/inbox.log; starts
//           the tasks listed in $FAKE_TASKS; runs $FAKE_DIR/cmd when
//           it appears, as a command the session itself issued
//   tail    a `tail -f` that never reads and waits
//   loop    a background task that never writes
//   chatty  a background task that writes a line every second
//
// Compiled rather than scripted: the watch reads each process's
// environment, working directory and standard output, and the system's
// own interpreters keep those to themselves.  Every one of them exits
// after $FAKE_SECONDS (default 900), whatever the test does.
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/un.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

static const char *base(const char *p) {
    const char *s = strrchr(p, '/');
    return s ? s + 1 : p;
}

static time_t deadline(void) {
    const char *s = getenv("FAKE_SECONDS");
    return time(0) + (s ? atoi(s) : 900);
}

static const char *dir;
static FILE *log_f;

// a task: its own session, standard output to `out` (a task's output
// file, dated a day back as if long silent), the command exec'd by sh
static void spawn(const char *id, const char *cmd) {
    char out[PATH_MAX];
    int task = strcmp(id, "-") != 0;
    if (task) snprintf(out, sizeof out, "%s/tasks/%s.output", dir, id);
    else snprintf(out, sizeof out, "/dev/null");
    pid_t pid = fork();
    if (pid == 0) {
        setsid();
        int fd = open(out, O_WRONLY | O_CREAT | O_TRUNC, 0644);
        if (fd < 0) _exit(126);
        if (task) {
            struct timeval old[2] = {{time(0) - 86400, 0}, {time(0) - 86400, 0}};
            futimes(fd, old);
        }
        dup2(fd, 1);
        dup2(fd, 2);
        execl("/bin/sh", "sh", "-c", cmd, (char *)0);
        _exit(127);
    }
    fprintf(log_f, "task %s pid=%d\n", id, (int)pid);
    fflush(log_f);
}

static void start_tasks(void) {
    const char *list = getenv("FAKE_TASKS");
    FILE *t = list ? fopen(list, "r") : 0;
    char line[4096];
    while (t && fgets(line, sizeof line, t)) {
        line[strcspn(line, "\n")] = 0;
        char *tab = strchr(line, '\t');
        if (!tab) continue;
        *tab = 0;
        spawn(line, tab + 1);
    }
    if (t) fclose(t);
}

static void take(int listener) {
    int c = accept(listener, 0, 0);
    if (c < 0) return;
    char path[PATH_MAX], buf[65536];
    snprintf(path, sizeof path, "%s/inbox.log", dir);
    FILE *f = fopen(path, "a");
    struct timeval tv = {5, 0};
    setsockopt(c, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    ssize_t n;
    while ((n = read(c, buf, sizeof buf)) > 0)
        if (f) fwrite(buf, 1, (size_t)n, f);
    if (f) fclose(f);
    close(c);
}

static void run_cmd(void) {
    char cmd[PATH_MAX], run[PATH_MAX], line[PATH_MAX * 3];
    snprintf(cmd, sizeof cmd, "%s/cmd", dir);
    snprintf(run, sizeof run, "%s/cmd.run", dir);
    if (rename(cmd, run) != 0) return;
    snprintf(line, sizeof line, "/bin/sh '%s' > '%s/cmd.out' 2>&1; echo $? > '%s/cmd.status'", run, dir, dir);
    system(line);
}

static int claude(time_t end) {
    const char *uuid = getenv("FAKE_UUID"), *cfg = getenv("CLAUDE_CONFIG_DIR");
    dir = getenv("FAKE_DIR");
    if (!dir || !uuid || !cfg) return 9;
    char path[PATH_MAX], cwd[PATH_MAX], rec[PATH_MAX], sock[104];
    snprintf(path, sizeof path, "%s/claude.log", dir);
    if (!(log_f = fopen(path, "a"))) return 9;
    snprintf(path, sizeof path, "%s/work", dir);
    mkdir(path, 0755);
    if (chdir(path) != 0 || !getcwd(cwd, sizeof cwd)) return 9;
    setenv("CLAUDE_CODE_SESSION_ID", uuid, 1);
    snprintf(sock, sizeof sock, "%s/i%d.sock", dir, (int)getpid());
    int l = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a = {.sun_family = AF_UNIX};
    snprintf(a.sun_path, sizeof a.sun_path, "%s", sock);
    unlink(sock);
    if (l < 0 || bind(l, (struct sockaddr *)&a, sizeof a) != 0 || listen(l, 8) != 0) return 9;
    snprintf(path, sizeof path, "%s/sessions", cfg);
    mkdir(path, 0755);
    snprintf(rec, sizeof rec, "%s/sessions/%d.json", cfg, (int)getpid());
    FILE *r = fopen(rec, "w");
    if (!r) return 9;
    fprintf(r, "{\"pid\":%d,\"sessionId\":\"%s\",\"cwd\":\"%s\",\"kind\":\"interactive\",\"entrypoint\":\"cli\",\"messagingSocketPath\":\"%s\"}",
            (int)getpid(), uuid, cwd, sock);
    fclose(r);
    fprintf(log_f, "start pid=%d cwd=%s\n", (int)getpid(), cwd);
    fflush(log_f);
    printf("fake claude %d\n", (int)getpid());
    fflush(stdout);
    start_tasks();
    while (time(0) < end) {
        fd_set fds;
        FD_ZERO(&fds);
        FD_SET(l, &fds);
        struct timeval tv = {0, 500000};
        if (select(l + 1, &fds, 0, 0, &tv) > 0) take(l);
        run_cmd();
        while (waitpid(-1, 0, WNOHANG) > 0) {}
    }
    unlink(rec);
    unlink(sock);
    return 0;
}

int main(int argc, char **argv) {
    (void)argc;
    const char *me = base(argv[0]);
    time_t end = deadline();
    if (!strcmp(me, "claude")) return claude(end);
    if (!strcmp(me, "chatty")) {
        while (time(0) < end) {
            printf("tick %ld\n", (long)time(0));
            fflush(stdout);
            sleep(1);
        }
        return 0;
    }
    while (time(0) < end) sleep(1);
    return 0;
}
