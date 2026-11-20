import {
  invoke as nativeInvoke,
  type InvokeArgs,
  type InvokeOptions,
} from "@tauri-apps/api/core";
import type { Event, EventCallback } from "@tauri-apps/api/event";
import type { AppId } from "@/lib/api/types";

export * from "@tauri-apps/api/core";

function remoteConfigurationTarget() {
  if (
    typeof window === "undefined" ||
    !window.location.pathname.endsWith("/provider-machine.html")
  ) {
    return undefined;
  }
  const params = new URLSearchParams(window.location.search);
  const hostId = params.get("host");
  const app = params.get("app");
  if (!hostId || !app) return undefined;
  return { hostId, app: app as AppId, name: params.get("name") ?? hostId };
}

export const remoteConfiguration = remoteConfigurationTarget();

// The target belongs to this renderer for its lifetime. Changing the selected
// machine mounts another renderer and never changes the target of pending work.
export function invokeLocal<T>(
  command: string,
  args?: InvokeArgs,
  options?: InvokeOptions,
): Promise<T> {
  if (remoteConfiguration && window.parent !== window) {
    const parentIpc = (
      window.parent as Window & {
        __TAURI_INTERNALS__?: { invoke: typeof nativeInvoke };
      }
    ).__TAURI_INTERNALS__;
    if (parentIpc) return parentIpc.invoke<T>(command, args, options);
  }
  if (options !== undefined) return nativeInvoke<T>(command, args, options);
  if (args !== undefined) return nativeInvoke<T>(command, args);
  return nativeInvoke<T>(command);
}

const localActions = new Set([
  "open_external",
  "copy_text_to_clipboard",
  "report_frontend_error",
  "log_frontend_error",
  "plugin:opener|open_url",
]);

type MachineEvent = { event: string; payload: unknown };
type MachineReply<T> = { data?: T; error?: unknown; events: MachineEvent[] };

let nextEventId = 0;
const machineListeners = new Map<
  number,
  {
    event: string;
    callback: EventCallback<unknown>;
  }
>();

export function listenOnMachine<T>(event: string, callback: EventCallback<T>) {
  const id = ++nextEventId;
  machineListeners.set(id, {
    event,
    callback: callback as EventCallback<unknown>,
  });
  return () => {
    machineListeners.delete(id);
  };
}

export function emitOnMachine<T>(event: string, payload: T) {
  for (const [id, listener] of machineListeners) {
    if (listener.event === event) {
      const message: Event<T> = { event, id, payload };
      listener.callback(message);
    }
  }
}

export async function invoke<T = unknown>(
  command: string,
  args?: InvokeArgs,
  options?: InvokeOptions,
): Promise<T> {
  if (!remoteConfiguration || localActions.has(command)) {
    return invokeLocal<T>(command, args, options);
  }
  if (command === "open_provider_terminal") {
    return invokeLocal<T>("ssh_open_provider_terminal", {
      hostId: remoteConfiguration.hostId,
      args: args ?? {},
    });
  }
  const reply = await invokeLocal<MachineReply<T>>("ssh_provider_command", {
    hostId: remoteConfiguration.hostId,
    command,
    args: args ?? {},
  });
  for (const event of reply.events ?? []) {
    emitOnMachine(event.event, event.payload);
  }
  if ("error" in reply) throw reply.error;
  return reply.data as T;
}
