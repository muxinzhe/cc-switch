import { afterEach, describe, expect, it, vi } from "vitest";
import { invoke as nativeInvoke } from "@tauri-apps/api/core";

vi.mock("@tauri-apps/api/core", () => ({ invoke: vi.fn() }));

const originalPath = `${window.location.pathname}${window.location.search}`;

afterEach(() => {
  window.history.replaceState(null, "", originalPath);
  vi.resetModules();
});

describe("machine-scoped provider IPC", () => {
  it("uses the normal commands for local configurations", async () => {
    vi.mocked(nativeInvoke).mockResolvedValueOnce({});
    const { invoke } = await import("@/lib/machineIpc");
    await invoke("get_providers", { app: "codex" });
    expect(nativeInvoke).toHaveBeenCalledWith("get_providers", {
      app: "codex",
    });
  });

  it("binds reads and edits to a host across renderer navigation", async () => {
    window.history.replaceState(
      null,
      "",
      "/provider-machine.html?host=host-a&app=codex",
    );
    const { invoke, listenOnMachine } = await import("@/lib/machineIpc");
    const onSwitched = vi.fn();
    const unlisten = listenOnMachine("provider-switched", onSwitched);
    vi.mocked(nativeInvoke).mockResolvedValueOnce({
      data: true,
      events: [
        { event: "provider-switched", payload: { providerId: "example" } },
      ],
    });
    window.history.replaceState(
      null,
      "",
      "/provider-machine.html?host=host-b&app=codex",
    );
    const args = { app: "codex", provider: { id: "example" } };
    await expect(invoke("update_provider", args)).resolves.toBe(true);
    expect(nativeInvoke).toHaveBeenLastCalledWith("ssh_provider_command", {
      hostId: "host-a",
      command: "update_provider",
      args,
    });
    expect(onSwitched).toHaveBeenCalledWith(
      expect.objectContaining({
        event: "provider-switched",
        payload: { providerId: "example" },
      }),
    );
    unlisten();

    vi.mocked(nativeInvoke).mockResolvedValueOnce({
      error: "Unsupported command",
    });
    await expect(
      invoke("delete_provider", { app: "codex", id: "example" }),
    ).rejects.toBe("Unsupported command");
    expect(nativeInvoke).not.toHaveBeenCalledWith(
      "delete_provider",
      expect.anything(),
    );
  });
});
