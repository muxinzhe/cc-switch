import { useState } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { Copy } from "lucide-react";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { invokeLocal, remoteConfiguration } from "@/lib/machineIpc";
import { providersApi, type AppId } from "@/lib/api";
import { toast } from "@/lib/toast";
import type { Provider } from "@/types";
import { MachineError } from "./MachineScope";

export function SyncProviderButton({
  app,
  current,
  disabled,
}: {
  app: AppId;
  current: Record<string, Provider>;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const client = useQueryClient();
  const [open, setOpen] = useState(false);
  const [selected, setSelected] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<unknown>();
  const source = useQuery({
    queryKey: ["local-provider-copy-source", app],
    queryFn: () =>
      invokeLocal<Record<string, Provider>>("get_providers", { app }),
    enabled: open,
    retry: false,
    gcTime: 0,
  });

  const sourceProvider = selected ? source.data?.[selected] : undefined;
  const existing = sourceProvider ? current[sourceProvider.id] : undefined;
  const label = t("ssh.syncLocalProvider", {
    defaultValue: "Sync from this computer",
  });

  const sync = async () => {
    if (!sourceProvider) return;
    setBusy(true);
    setError(undefined);
    try {
      if (existing) await providersApi.update(sourceProvider, app);
      else await providersApi.add(sourceProvider, app, false);
      await client.invalidateQueries({ queryKey: ["providers", app] });
      toast.success(
        t("ssh.providerSynced", { defaultValue: "Provider synced" }),
      );
      setOpen(false);
      setSelected(undefined);
    } catch (cause) {
      setError(cause);
    } finally {
      setBusy(false);
    }
  };

  return (
    <>
      <Button
        variant="quiet"
        size="regular"
        disabled={disabled}
        onClick={() => setOpen(true)}
      >
        <Copy aria-hidden="true" className="size-4" />
        {label}
      </Button>
      <Dialog
        open={open}
        onOpenChange={(next) => {
          if (!busy) setOpen(next);
        }}
      >
        <DialogContent className="max-w-lg">
          <DialogHeader>
            <DialogTitle>{label}</DialogTitle>
            <DialogDescription>
              {t("ssh.syncLocalProviderDetails", {
                defaultValue:
                  "Copy the selected provider and its configuration to {{host}}.",
                host: remoteConfiguration?.name,
              })}
            </DialogDescription>
          </DialogHeader>
          <div className="space-y-3 px-6 pb-4">
            {source.isLoading && <p role="status">{t("common.loading")}</p>}
            {!!(error || source.error) && (
              <MachineError error={error || source.error} />
            )}
            {source.data && Object.keys(source.data).length === 0 && (
              <p className="text-caption text-fg-2">
                {t("ssh.noLocalProviders", {
                  defaultValue: "No providers on this computer",
                })}
              </p>
            )}
            {source.data && Object.keys(source.data).length > 0 && (
              <Select
                value={selected}
                onValueChange={setSelected}
                disabled={busy}
              >
                <SelectTrigger aria-label={t("usage.provider")}>
                  <SelectValue placeholder={t("usage.provider")} />
                </SelectTrigger>
                <SelectContent>
                  {Object.values(source.data).map((provider) => (
                    <SelectItem key={provider.id} value={provider.id}>
                      {provider.name}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            )}
            {existing && (
              <p className="text-caption text-fg-2">
                {t("ssh.replaceExistingProvider", {
                  defaultValue: "Saving will update {{name}} on this server.",
                  name: existing.name,
                })}
              </p>
            )}
          </div>
          <DialogFooter>
            <Button
              variant="neutral"
              disabled={busy}
              onClick={() => setOpen(false)}
            >
              {t("common.cancel")}
            </Button>
            <Button
              variant="solid"
              disabled={!sourceProvider || busy}
              onClick={() => void sync()}
            >
              {t("common.save")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </>
  );
}
