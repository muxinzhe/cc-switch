import { lazy, Suspense, useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { FileCode2 } from "lucide-react";
import { ConfirmDialog } from "@/components/ConfirmDialog";
import { FullScreenPanel } from "@/components/common/FullScreenPanel";
import { Button } from "@/components/ui/button";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import { useDarkMode } from "@/hooks/useDarkMode";
import { toast } from "@/lib/toast";
import type { AppId } from "@/lib/api";
import { MachineError } from "./MachineScope";
import { sshApi, sshKeys, type ConfigFileKey, type SshHost } from "./api";

const JsonEditor = lazy(() => import("@/components/JsonEditor"));

interface ConfigFileChoice {
  key: ConfigFileKey;
  name: string;
  json?: boolean;
}

const FILES: Partial<Record<AppId, ConfigFileChoice[]>> = {
  claude: [{ key: "claude", name: "settings.json", json: true }],
  codex: [{ key: "codex", name: "config.toml" }],
  gemini: [
    { key: "geminiSettings", name: "settings.json", json: true },
    { key: "geminiEnv", name: ".env" },
  ],
  opencode: [{ key: "openCode", name: "opencode.json / opencode.jsonc" }],
  openclaw: [{ key: "openClaw", name: "openclaw.json" }],
  hermes: [{ key: "hermes", name: "config.yaml" }],
  pi: [{ key: "pi", name: "models.json", json: true }],
};

export function RemoteConfigButton({
  app,
  host,
  onSelect,
}: {
  app: AppId;
  host: SshHost;
  onSelect: (host: SshHost, file: ConfigFileChoice) => void;
}) {
  const { t } = useTranslation();
  const files = FILES[app];
  if (!files?.length) return null;

  const label = t("ssh.configFiles", { defaultValue: "Configuration files" });
  const trigger = (
    <Button
      variant="quiet"
      size="compact"
      className="ms-auto shrink-0 gap-1.5"
      type="button"
      onClick={files.length === 1 ? () => onSelect(host, files[0]) : undefined}
    >
      <FileCode2 aria-hidden="true" className="size-4" />
      {label}
    </Button>
  );
  if (files.length === 1) return trigger;

  return (
    <DropdownMenu>
      <DropdownMenuTrigger asChild>{trigger}</DropdownMenuTrigger>
      <DropdownMenuContent align="end">
        {files.map((file) => (
          <DropdownMenuItem
            key={file.key}
            onSelect={() => onSelect(host, file)}
          >
            {file.name}
          </DropdownMenuItem>
        ))}
      </DropdownMenuContent>
    </DropdownMenu>
  );
}

export type RemoteConfigSelection = {
  host: SshHost;
  app: AppId;
  file: ConfigFileChoice;
};

export function RemoteConfigEditor({
  selection: { host, app, file },
  onClose,
}: {
  selection: RemoteConfigSelection;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const darkMode = useDarkMode();
  const queryClient = useQueryClient();
  const [draft, setDraft] = useState<string>();
  const [localPath, setLocalPath] = useState<string>();
  const [busy, setBusy] = useState<"load" | "save">();
  const [confirm, setConfirm] = useState<"save" | "discard">();
  const [error, setError] = useState<unknown>();
  const remote = useQuery({
    queryKey: [...sshKeys.host(host.id), "config-file", file.key],
    queryFn: () => sshApi.readConfigFile(host.id, file.key),
    gcTime: 0,
    retry: false,
    refetchOnWindowFocus: false,
    refetchOnReconnect: false,
  });
  const config = remote.data;
  const value = draft ?? config?.contents ?? "";
  const dirty = draft !== undefined && draft !== config?.contents;

  const loadLocal = async () => {
    setBusy("load");
    setError(undefined);
    try {
      const local = await sshApi.readLocalConfigFile(file.key);
      setDraft(local.contents);
      setLocalPath(local.path);
    } catch (error) {
      setError(error);
    } finally {
      setBusy(undefined);
    }
  };

  const save = async () => {
    if (!config || !dirty) return;
    setConfirm(undefined);
    setBusy("save");
    setError(undefined);
    try {
      const result = await sshApi.writeConfigFile(host.id, file.key, {
        contents: value,
        revision: config.revision,
      });
      void queryClient.invalidateQueries({
        queryKey: [...sshKeys.host(host.id), "providers", app],
      });
      toast.success(
        t("ssh.configSaved", { defaultValue: "Server configuration saved" }),
        result.backupPath
          ? {
              description: t("ssh.configBackup", {
                defaultValue: "Original file backed up to {{path}}",
                path: result.backupPath,
              }),
            }
          : undefined,
      );
      onClose();
    } catch (error) {
      setError(error);
    } finally {
      setBusy(undefined);
    }
  };

  const requestClose = () => {
    if (busy) return;
    if (dirty) setConfirm("discard");
    else onClose();
  };

  return (
    <>
      <FullScreenPanel
        isOpen
        title={t("ssh.editRemoteConfig", {
          defaultValue: "Configuration on {{host}}",
          host: host.name,
        })}
        onClose={requestClose}
        footer={
          <div className="flex w-full flex-wrap items-center justify-end gap-2">
            <Button variant="neutral" disabled={!!busy} onClick={requestClose}>
              {t("common.cancel")}
            </Button>
            <Button
              variant="solid"
              disabled={!!busy || !dirty || !config}
              onClick={() => setConfirm("save")}
            >
              {t("ssh.saveRemoteConfig", { defaultValue: "Save to server" })}
            </Button>
          </div>
        }
      >
        <div className="space-y-4">
          {remote.isPending && <p role="status">{t("common.loading")}</p>}
          {remote.isError && <MachineError error={remote.error} />}
          {!!error && <MachineError error={error} />}
          {config && (
            <>
              <div className="flex flex-wrap items-center justify-between gap-3">
                <code className="break-all text-caption text-fg-2">
                  {config.path}
                </code>
                <Button
                  variant="neutral"
                  disabled={!!busy}
                  onClick={() => void loadLocal()}
                >
                  {t("ssh.loadLocalConfig", {
                    defaultValue: "Load file from this computer",
                  })}
                </Button>
              </div>
              {!config.exists && (
                <p className="text-caption text-fg-2">
                  {t("ssh.configWillBeCreated", {
                    defaultValue: "Saving will create this file on the server.",
                  })}
                </p>
              )}
              {localPath && (
                <p className="break-all text-caption text-fg-2">
                  {t("ssh.configFromLocal", {
                    defaultValue:
                      "Loaded from {{path}}. Review before saving to the server.",
                    path: localPath,
                  })}
                </p>
              )}
              <Suspense fallback={<p role="status">{t("common.loading")}</p>}>
                <JsonEditor
                  ariaLabel={config.path}
                  value={value}
                  onChange={setDraft}
                  darkMode={darkMode}
                  language={file.json ? "json" : "javascript"}
                  showValidation={!!file.json}
                  height="min(52vh, 650px)"
                  readOnly={!!busy}
                />
              </Suspense>
            </>
          )}
        </div>
      </FullScreenPanel>
      <ConfirmDialog
        isOpen={confirm === "save"}
        variant="info"
        title={t("ssh.saveRemoteConfig", { defaultValue: "Save to server" })}
        message={t("ssh.confirmRemoteConfig", {
          defaultValue:
            "Save {{path}} on {{host}}? This file may contain API keys. If the file already exists, CC Switch saves a backup on the server.",
          host: host.name,
          path: config?.path,
        })}
        confirmText={t("common.save")}
        onConfirm={() => void save()}
        onCancel={() => setConfirm(undefined)}
      />
      <ConfirmDialog
        isOpen={confirm === "discard"}
        title={t("common.unsavedLeaveTitle")}
        message={t("common.unsavedLeaveMessage")}
        confirmText={t("common.unsavedLeaveConfirm")}
        cancelText={t("common.unsavedLeaveCancel")}
        onConfirm={onClose}
        onCancel={() => setConfirm(undefined)}
      />
    </>
  );
}
