import {
  emit as nativeEmit,
  emitTo as nativeEmitTo,
  listen as nativeListen,
  type EventCallback,
  type EventName,
  type EventTarget,
  type Options,
} from "@tauri-apps/api/event";
import {
  emitOnMachine,
  listenOnMachine,
  remoteConfiguration,
} from "./machineIpc";

export * from "@tauri-apps/api/event";

export async function listen<T>(
  event: EventName,
  handler: EventCallback<T>,
  options?: Options,
) {
  if (remoteConfiguration) return listenOnMachine(event, handler);
  return options === undefined
    ? nativeListen(event, handler)
    : nativeListen(event, handler, options);
}

export async function emit<T>(event: string, payload?: T) {
  if (remoteConfiguration) emitOnMachine(event, payload);
  else if (payload === undefined) await nativeEmit(event);
  else await nativeEmit(event, payload);
}

export async function emitTo<T>(
  target: EventTarget | string,
  event: string,
  payload?: T,
) {
  if (remoteConfiguration) emitOnMachine(event, payload);
  else if (payload === undefined) await nativeEmitTo(target, event);
  else await nativeEmitTo(target, event, payload);
}
