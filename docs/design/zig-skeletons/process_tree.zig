//! Skeleton: process-tree kill (MCP stdio server, subagent).
//!
//! Contract from docs/design/porting-traps.md 8: a server that exits cleanly
//! reaps only the direct child; grandchildren survive unless killed via the
//! process group. Kill the group while the pgid still belongs to us (the
//! child is unreaped, so its pid cannot be reused), then reap the direct
//! child. Syscall faces verified with `zig test` on 0.16.0; the 0.17 std.Io
//! layer owns fd plumbing, not process control (see README).
const std = @import("std");

pub const ProcTree = struct {
    pid: std.posix.pid_t,

    pub const SpawnError = error{ForkFailed};

    /// fork + setpgid(0,0) in the child, then exec. stdio is inherited; wire
    /// real pipes with dup2 in the child in your version (0.17 moved fd
    /// plumbing under std.Io, so keep this layer to fork/setpgid/dup2/exec).
    pub fn spawn(argv: [*:null]const ?[*:0]const u8, envp: [*:null]const ?[*:0]const u8) SpawnError!ProcTree {
        const pid = std.c.fork();
        if (pid < 0) return error.ForkFailed;
        if (pid == 0) {
            _ = std.c.setpgid(0, 0);
            _ = std.c.execve(argv[0].?, argv, envp);
            std.c._exit(127);
        }
        _ = std.c.setpgid(pid, pid); // close the race with a fast exec
        return .{ .pid = pid };
    }

    /// True while any member of the group exists (SIG 0 probe).
    pub fn groupAlive(self: *ProcTree) bool {
        if (std.posix.kill(-self.pid, @enumFromInt(0))) |_| return true else |err| {
            return err != std.posix.KillError.ProcessNotFound;
        }
    }

    /// TERM the whole group, wait up to `grace_ms`, then KILL it; reap the
    /// direct child so it cannot become a zombie. Idempotent.
    pub fn killTree(self: *ProcTree, grace_ms: u64) void {
        std.posix.kill(-self.pid, std.posix.SIG.TERM) catch {};
        var waited: u64 = 0;
        while (waited < grace_ms) : (waited += 10) {
            if (!self.groupAlive()) break;
            sleepMs(10);
        }
        std.posix.kill(-self.pid, std.posix.SIG.KILL) catch {};
        _ = std.c.waitpid(self.pid, null, 0);
    }
};

fn sleepMs(ms: u64) void {
    // 0.16 moved thread sleep under std.Io; poll with an empty fd set is the
    // stable portable primitive (and is exactly what a real wait loop does
    // when it is also watching the child).
    _ = std.posix.poll(&[_]std.posix.pollfd{}, @intCast(ms)) catch {};
}

const testing = std.testing;

test "killTree empties the process group built with setpgid" {
    // The background child inherits the shell group; killing the group must
    // reach it even though it is a grandchild of this test.
    const argv = [_:null]?[*:0]const u8{ "/bin/sh", "-c", "sleep 30 & wait" };
    // A minimal, explicit environment: the child must not inherit provider
    // keys or proxy settings. This whitelist is part of the contract.
    const envp = [_:null]?[*:0]const u8{"PATH=/usr/bin:/bin"};
    var tree = try ProcTree.spawn(&argv, &envp);
    var tries: usize = 0;
    while (tries < 200) : (tries += 1) {
        if (tree.groupAlive()) break;
        sleepMs(10);
    }
    try testing.expect(tree.groupAlive());
    tree.killTree(500);
    sleepMs(100);
    try testing.expect(!tree.groupAlive());
}
