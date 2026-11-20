import { useEffect, useRef } from "react";
import { useQuery } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import { invoke } from "@/lib/machineIpc";
import type { AppId } from "@/lib/api";
import { MachineError } from "./MachineScope";
import { sshKeys, type SshHost } from "./api";

const FRAME_ID = "remote-provider-workspace";
const NAVIGATION_TYPE = "cc-switch:provider-machine-navigation";
export type ProviderMachinePage = "usage" | "sessions" | "apps";

export function navigateFromProviderMachine(page: ProviderMachinePage) {
  window.parent.postMessage({ type: NAVIGATION_TYPE, page }, "*");
}

export function providerFrameDrafts() {
  const frame = document.getElementById(FRAME_ID) as HTMLIFrameElement | null;
  return frame?.contentWindow?.__ccSwitchProviderDrafts;
}

declare global {
  interface Window {
    __ccSwitchProviderDrafts?: { dirty: () => boolean; discard: () => void };
  }
}

export function ProviderMachineFrame({
  host,
  app,
  onNavigate,
}: {
  host: SshHost;
  app: AppId;
  onNavigate?: (page: ProviderMachinePage) => void;
}) {
  const { t } = useTranslation();
  const frame = useRef<HTMLIFrameElement>(null);
  useEffect(() => {
    const receive = (event: MessageEvent) => {
      const child = frame.current?.contentWindow;
      if (!child || event.source !== child) return;
      if (event.data?.type !== NAVIGATION_TYPE) return;
      const page = event.data.page;
      if (page === "usage" || page === "sessions" || page === "apps") {
        onNavigate?.(page);
      }
    };
    window.addEventListener("message", receive);
    return () => window.removeEventListener("message", receive);
  }, [onNavigate]);
  const connected = useQuery({
    queryKey: [...sshKeys.host(host.id), "configuration"],
    queryFn: async () => {
      const reply = await invoke<{
        data?: { protocol: number };
        error?: unknown;
      }>("ssh_provider_command", {
        hostId: host.id,
        command: "hello",
        args: {},
      });
      if (!reply.data)
        throw (
          reply.error ?? new Error("SSH configuration backend is unavailable")
        );
      return reply.data;
    },
    retry: false,
    refetchOnWindowFocus: false,
    staleTime: Infinity,
  });
  if (connected.isError) {
    return (
      <div className="space-y-3 px-6 py-4">
        <MachineError error={connected.error} />
        <Button variant="neutral" onClick={() => void connected.refetch()}>
          {t("common.retry")}
        </Button>
      </div>
    );
  }
  if (!connected.data) {
    return (
      <p role="status" className="px-6 py-4 text-caption text-fg-2">
        {t("common.loading")}
      </p>
    );
  }
  const query = new URLSearchParams({ host: host.id, app, name: host.name });
  return (
    <iframe
      id={FRAME_ID}
      ref={frame}
      key={`${host.id}:${app}`}
      src={`./provider-machine.html?${query}`}
      title={host.name}
      className="min-h-0 w-full flex-1 border-0 bg-app"
    />
  );
}
