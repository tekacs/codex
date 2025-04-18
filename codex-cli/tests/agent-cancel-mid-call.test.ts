// Regression test: cancelling *after* the model has emitted a function_call
// but *before* the corresponding `response.completed` message arrived should
// *not* lead to synthetic `function_call_output` items being sent in the next
// turn (because `previous_response_id` is unknown at that point). Such
// stubs would trigger
//   400 | No tool output found for function call …
// errors from the OpenAI API. The agent should instead quietly drop the
// dangling call IDs.

import { describe, it, expect, vi } from "vitest";

// --- Mock OpenAI -----------------------------------------------------------

// Stream that delivers a function_call quickly and then stalls so the test can
// cancel before `response.completed` is produced.
class MidCallStream {
  private _aborted = false;

  public controller = {
    abort: () => {
      this._aborted = true;
    },
  };

  async *[Symbol.asyncIterator]() {
    // Give the agent a tick to start listening.
    await new Promise((r) => setTimeout(r, 5));

    // Emit the function_call (no response.completed afterwards).
    yield {
      type: "response.output_item.done",
      item: {
        type: "function_call",
        id: "mid_call",
        name: "shell",
        arguments: JSON.stringify({ cmd: ["echo", "hi"] }),
      },
    } as any;

    // Keep the generator alive until aborted so the agent has to cancel.
    while (!this._aborted) {
      await new Promise((r) => setTimeout(r, 5));
    }
  }
}

vi.mock("openai", () => {
  const bodies: Array<any> = [];
  let invocation = 0;

  class FakeOpenAI {
    public responses = {
      create: async (body: any) => {
        bodies.push(body);
        invocation += 1;
        if (invocation === 1) {
          // First request returns the stream that will be cancelled midway.
          return new MidCallStream();
        }
        // Subsequent requests: empty stream.
        return new (class {
          public controller = { abort: vi.fn() };
          async *[Symbol.asyncIterator]() {
            /* no events */
          }
        })();
      },
    };
  }

  class APIConnectionTimeoutError extends Error {}

  return {
    __esModule: true,
    default: FakeOpenAI,
    APIConnectionTimeoutError,
    _test: {
      getBodies: () => bodies,
    },
  };
});

// --- Stub utilities that are irrelevant for the behaviour under test -------

vi.mock("../src/approvals.js", () => ({
  __esModule: true,
  alwaysApprovedCommands: new Set<string>(),
  canAutoApprove: () => ({ type: "auto-approve", runInSandbox: false } as any),
}));

vi.mock("../src/format-command.js", () => ({
  __esModule: true,
  formatCommandForDisplay: (c: Array<string>) => c.join(" "),
}));

vi.mock("../src/utils/agent/log.js", () => ({
  __esModule: true,
  log: () => {},
  isLoggingEnabled: () => false,
}));

// --- Actual test -----------------------------------------------------------

import { AgentLoop } from "../src/utils/agent/agent-loop.js";

describe("cancel after function_call but before completed", () => {
  it(
    "does NOT send synthetic function_call_output in next run",
    async () => {
    const { _test } = (await import("openai")) as any;

    const agent = new AgentLoop({
      model: "any",
      instructions: "",
      approvalPolicy: { mode: "auto" } as any,
      additionalWritableRoots: [],
      onItem: () => {},
      onLoading: () => {},
      getCommandConfirmation: async () => ({ review: "yes" } as any),
      onLastResponseId: () => {},
      config: { model: "any", instructions: "", notify: false },
    });

    // Start first run (do not await – we'll cancel mid‑stream).
    const firstRun = agent.run([
      {
        type: "message",
        role: "user",
        content: [{ type: "input_text", text: "please run" }],
      },
    ] as any);

    // Wait until the function_call has likely been delivered.
    await new Promise((r) => setTimeout(r, 15));

    // Cancel before response.completed arrives.
    agent.cancel();

    // Ensure the first run finishes cleanly.
    await firstRun.catch(() => {});

    // Second run – this should *not* include a synthetic abort output.
    await agent.run([
      {
        type: "message",
        role: "user",
        content: [{ type: "input_text", text: "next" }],
      },
    ] as any);

    const bodies = _test.getBodies();
    expect(bodies.length).toBeGreaterThanOrEqual(2);

    const secondBody = bodies[bodies.length - 1];
    expect(Array.isArray(secondBody.input)).toBe(true);

    // Ensure no function_call_output referencing the cancelled call is present.
    const hasSynthetic = secondBody.input.some(
      (i: any) => i.type === "function_call_output" && i.call_id === "mid_call",
    );

    expect(hasSynthetic).toBe(false);
  },
    10000,
  );
});
