//! Skeleton: ACP turn dispatch — guard + spawn + cancel-before-registration.
//!
//! Contract from docs/design/porting-traps.md 1-3. Layout/semantics are the
//! point; names follow Zig 0.16.0, where `zig test` on this file passes. The
//! rewrite targets 0.17 (std.Io migration) - keep the invariants, adapt names.
const std = @import("std");

pub const TurnState = struct {
    in_flight: std.atomic.Value(bool) = .init(false),
    cancelled: std.atomic.Value(bool) = .init(false),

    /// True when the slot was claimed; the caller must `release` on every path.
    pub fn begin(self: *TurnState) bool {
        if (self.in_flight.swap(true, .acq_rel)) return false; // second concurrent prompt
        self.cancelled.store(false, .release); // AFTER the claim, never before
        return true;
    }

    pub fn release(self: *TurnState) void {
        self.in_flight.store(false, .release);
    }

    /// session/cancel: only meaningful while a turn is in flight.
    pub fn cancel(self: *TurnState) void {
        if (!self.in_flight.load(.acquire)) return; // idle cancel: no-op
        self.cancelled.store(true, .release);
    }

    pub fn isCancelled(self: *TurnState) bool {
        return self.cancelled.load(.acquire);
    }
};

/// One step of the agent loop (your provider call, a tool batch, ...).
pub const StepFn = *const fn (*Turn) void;

pub const Turn = struct {
    state: *TurnState,
    /// Set by the worker once its blocking operation exists (socket fd, child
    /// pipe, ...). Cancellation calls it to interrupt a blocked read; checking
    /// `isCancelled` between steps is the fallback.
    abort: ?*const fn () void = null,

    /// Cancel is observed before the first step and between steps. A
    /// pre-aborted start (cancel arrived before the worker was scheduled) must
    /// resolve as `aborted` instead of running the turn and then reporting
    /// cancelled.
    pub fn run(self: *Turn, steps: []const StepFn) Outcome {
        for (steps) |step| {
            if (self.state.isCancelled()) {
                if (self.abort) |abort| abort();
                return .aborted;
            }
            step(self);
        }
        if (self.state.isCancelled()) return .aborted;
        return .completed;
    }
};

pub const Outcome = enum { completed, aborted };

pub const TurnCtx = struct {
    state: *TurnState,
    steps: []const StepFn,
    onOutcome: *const fn (Outcome) void,
};

/// Dispatcher side: synchronous claim, spawn, return immediately.
pub fn dispatchTurn(ctx: TurnCtx) !bool {
    if (!ctx.state.begin()) return false; // a turn is already running
    const owned = try std.heap.page_allocator.create(TurnCtx);
    owned.* = ctx;
    const t = try std.Thread.spawn(.{}, worker, .{owned});
    t.detach();
    return true;
}

fn worker(ctx: *TurnCtx) void {
    defer std.heap.page_allocator.destroy(ctx);
    defer ctx.state.release(); // every exit path, including an error return
    var turn = Turn{ .state = ctx.state };
    const outcome = turn.run(ctx.steps);
    ctx.onOutcome(outcome); // exactly once, after final events were sent
}

const testing = std.testing;

fn noopStep(_: *Turn) void {}

test "same-burst cancel is not lost: pre-aborted start" {
    var state = TurnState{};
    try testing.expect(state.begin());
    state.cancel(); // cancel lands before the worker is scheduled
    const steps = [_]StepFn{noopStep} ** 64;
    var turn = Turn{ .state = &state };
    try testing.expectEqual(Outcome.aborted, turn.run(&steps));
    state.release();
}

test "idle cancel does not poison the next turn" {
    var state = TurnState{};
    state.cancel();
    try testing.expect(!state.isCancelled());
    try testing.expect(state.begin());
    try testing.expect(!state.isCancelled());
    state.release();
}
