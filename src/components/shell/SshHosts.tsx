import { useState, type FormEvent } from "react";
import { ChevronDown, Laptop, Loader2, Plus, Server } from "lucide-react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { HoverTip } from "@/components/ui/hover-tip";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { Switch } from "@/components/ui/switch";
import { sshApi, sshKeys, useSshHosts, type SshHost } from "@/ssh/api";
import { cn } from "@/lib/utils";
import { extractErrorMessage } from "@/utils/errorUtils";

export function SshHosts({
  collapsed,
  onExpand,
}: {
  collapsed: boolean;
  onExpand: () => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const { data: hosts = [], error: hostsError } = useSshHosts();
  const [expanded, setExpanded] = useState(false);
  const [editor, setEditor] = useState<SshHost | "new" | null>(null);
  const [busy, setBusy] = useState<string>();
  const [error, setError] = useState("");

  async function updateEnabled(host: SshHost, enabled: boolean) {
    setBusy(host.id);
    setError("");
    try {
      await sshApi.saveHost({ ...host, enabled });
      await queryClient.invalidateQueries({ queryKey: sshKeys.hosts });
      await queryClient.invalidateQueries({ queryKey: sshKeys.host(host.id) });
      if (enabled) await sshApi.connect(host.id);
    } catch (cause) {
      setError(`${host.name} · ${extractErrorMessage(cause)}`);
    } finally {
      setBusy(undefined);
    }
  }

  return (
    <div className="shrink-0 px-2 pb-2">
      <HoverTip content={collapsed ? t("ssh.title") : undefined} side="right">
        <button
          className={cn(
            "flex h-8 w-full items-center gap-2 rounded-control px-2 text-caption text-fg-2 hover:bg-subtle hover:text-fg-1",
            collapsed && "justify-center",
          )}
          aria-label={t("ssh.title")}
          aria-expanded={expanded && !collapsed}
          type="button"
          onClick={() => {
            if (collapsed) {
              onExpand();
              setExpanded(true);
            } else {
              setExpanded((value) => !value);
            }
          }}
        >
          <Laptop className="size-4 shrink-0" />
          {!collapsed && (
            <>
              <span className="min-w-0 flex-1 truncate text-start">
                {t("ssh.title")}
              </span>
              <ChevronDown
                className={cn("size-3 shrink-0", expanded && "rotate-180")}
              />
            </>
          )}
        </button>
      </HoverTip>
      {expanded && !collapsed && (
        <div className="space-y-1 py-1">
          <ul className="max-h-36 space-y-0.5 overflow-y-auto">
            {hosts.map((host) => (
              <li
                key={host.id}
                className="flex items-center gap-1.5 rounded-control px-2"
              >
                <button
                  type="button"
                  className="flex min-w-0 flex-1 items-center gap-2 rounded-control py-1 text-start hover:text-action"
                  onClick={() => setEditor(host)}
                >
                  <Server className="size-3.5 shrink-0" />
                  <span className="truncate text-caption">{host.name}</span>
                </button>
                {busy === host.id ? (
                  <Loader2 className="size-3 animate-spin" />
                ) : (
                  <Switch
                    aria-label={`${t("ssh.enableBackend")} · ${host.name}`}
                    checked={host.enabled}
                    disabled={busy != null}
                    onCheckedChange={(enabled) =>
                      void updateEnabled(host, enabled)
                    }
                  />
                )}
              </li>
            ))}
          </ul>
          <button
            type="button"
            className="flex w-full items-center gap-2 rounded-control px-2 py-1 text-start text-caption text-fg-2 hover:bg-subtle"
            onClick={() => setEditor("new")}
          >
            <Plus className="size-3.5 shrink-0" />
            {t("ssh.addHost")}
          </button>
          {(error || hostsError) && (
            <p
              role="alert"
              className="whitespace-normal break-words px-2 text-caption text-destructive"
            >
              {error || extractErrorMessage(hostsError)}
            </p>
          )}
        </div>
      )}
      {editor && (
        <HostEditor
          host={editor === "new" ? undefined : editor}
          onFinished={() => {
            setEditor(null);
            setExpanded(true);
          }}
          onConnectionError={setError}
        />
      )}
    </div>
  );
}

function HostEditor({
  host,
  onFinished,
  onConnectionError,
}: {
  host?: SshHost;
  onFinished: () => void;
  onConnectionError: (error: string) => void;
}) {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const {
    data: aliases = [],
    isLoading: loadingAliases,
    error: aliasesError,
  } = useQuery({
    queryKey: ["ssh", "aliases"],
    queryFn: sshApi.aliases,
    retry: false,
  });
  const [name, setName] = useState(host?.name ?? "");
  const [destination, setDestination] = useState(host?.destination ?? "");
  const [port, setPort] = useState(host?.port?.toString() ?? "");
  const [identityFile, setIdentityFile] = useState(host?.identityFile ?? "");
  const [enabled, setEnabled] = useState(host?.enabled ?? true);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);

  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    setBusy(true);
    setError("");
    try {
      const saved = await sshApi.saveHost({
        id: host?.id,
        name,
        destination,
        port: port.trim() ? Number(port) : null,
        identityFile: identityFile.trim() || null,
        enabled,
      });
      await queryClient.invalidateQueries({ queryKey: sshKeys.hosts });
      await queryClient.invalidateQueries({ queryKey: sshKeys.host(saved.id) });
      onConnectionError("");
      if (enabled) {
        try {
          await sshApi.connect(saved.id);
        } catch (cause) {
          onConnectionError(`${saved.name} · ${extractErrorMessage(cause)}`);
        }
      }
      onFinished();
    } catch (cause) {
      setError(extractErrorMessage(cause));
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (!host) return;
    setBusy(true);
    try {
      await sshApi.removeHost(host.id);
      await queryClient.invalidateQueries({ queryKey: sshKeys.hosts });
      onFinished();
    } catch (cause) {
      setError(extractErrorMessage(cause));
      setBusy(false);
    }
  }

  return (
    <Dialog
      open
      onOpenChange={(open) => {
        if (!open && !busy) onFinished();
      }}
    >
      <DialogContent className="sm:max-w-md">
        <form onSubmit={save}>
          <DialogHeader>
            <DialogTitle>
              {host ? t("ssh.editHost") : t("ssh.addHost")}
            </DialogTitle>
            <DialogDescription>{t("ssh.description")}</DialogDescription>
          </DialogHeader>
          <div className="space-y-3 px-6 pb-2">
            <div className="space-y-1.5">
              <Label>{t("ssh.fromConfig")}</Label>
              <Select
                value={aliases.includes(destination) ? destination : ""}
                disabled={busy || aliases.length === 0}
                onValueChange={(alias) => {
                  setDestination(alias);
                  setPort("");
                  setIdentityFile("");
                }}
              >
                <SelectTrigger aria-label={t("ssh.fromConfig")}>
                  <SelectValue
                    placeholder={t(
                      loadingAliases
                        ? "common.loading"
                        : aliases.length
                          ? "ssh.fromConfig"
                          : "ssh.noAliases",
                    )}
                  />
                </SelectTrigger>
                <SelectContent>
                  {aliases.map((alias) => (
                    <SelectItem key={alias} value={alias}>
                      {alias}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
              {aliasesError && (
                <p className="text-caption text-destructive">
                  {extractErrorMessage(aliasesError)}
                </p>
              )}
            </div>
            <div className="grid grid-cols-[1fr_7rem] gap-2">
              <Label className="space-y-1.5">
                {t("ssh.destination")}
                <Input
                  autoFocus
                  value={destination}
                  placeholder={t("ssh.placeholder")}
                  required
                  disabled={busy}
                  onChange={(e) => setDestination(e.target.value)}
                />
              </Label>
              <Label className="space-y-1.5">
                {t("ssh.port")}
                <Input
                  type="number"
                  placeholder="22"
                  min={1}
                  max={65535}
                  value={port}
                  disabled={busy}
                  onChange={(e) => setPort(e.target.value)}
                />
              </Label>
            </div>
            <Label className="block space-y-1.5">
              {t("ssh.hostName")}
              <Input
                value={name}
                placeholder={destination}
                disabled={busy}
                onChange={(e) => setName(e.target.value)}
              />
            </Label>
            <Label className="block space-y-1.5">
              {t("ssh.identityFile")}
              <Input
                value={identityFile}
                placeholder="~/.ssh/id_ed25519"
                disabled={busy}
                onChange={(e) => setIdentityFile(e.target.value)}
              />
            </Label>
            <Label className="flex items-center gap-2">
              <Switch
                checked={enabled}
                disabled={busy}
                onCheckedChange={setEnabled}
              />
              {t("ssh.enableBackend")}
            </Label>
            {error && (
              <p role="alert" className="break-words text-sm text-destructive">
                {error}
              </p>
            )}
          </div>
          <DialogFooter>
            {host && (
              <Button
                type="button"
                className="me-auto"
                variant="outline"
                disabled={busy}
                onClick={() => void remove()}
              >
                {t("ssh.removeHost")}
              </Button>
            )}
            <Button
              type="button"
              variant="outline"
              disabled={busy}
              onClick={onFinished}
            >
              {t("common.cancel")}
            </Button>
            <Button type="submit" disabled={busy || !destination.trim()}>
              {busy && <Loader2 className="me-1 size-4 animate-spin" />}
              {t("common.save")}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  );
}
