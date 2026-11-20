import { useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from "@/components/ui/select";
import { extractErrorMessage } from "@/utils/errorUtils";
import { useSshHosts, type SshHost } from "./api";

export function useMachineScope(initialMachine = "all") {
  const { data, error, isPending } = useSshHosts();
  const [selected, select] = useState(initialMachine);
  const hosts = (data ?? []).filter((host) => host.enabled);
  const scope =
    selected === "local" || hosts.some((host) => host.id === selected)
      ? selected
      : "all";
  const remote =
    scope === "all" ? hosts : hosts.filter((host) => host.id === scope);
  return {
    hosts,
    scope,
    select,
    remote,
    error,
    pending: scope === "all" && isPending,
    includeLocal: scope === "all" || scope === "local",
  };
}

export function MachinePicker({
  hosts,
  scope,
  select,
  includeAll = true,
}: {
  hosts: SshHost[];
  scope: string;
  select: (id: string) => void;
  includeAll?: boolean;
}) {
  const { t } = useTranslation();
  return (
    <Select value={scope} onValueChange={select}>
      <SelectTrigger
        aria-label={t("ssh.machineFilter")}
        className="h-8 w-auto min-w-[9rem] max-w-60"
      >
        <SelectValue />
      </SelectTrigger>
      <SelectContent>
        {includeAll && (
          <SelectItem value="all">{t("ssh.allMachines")}</SelectItem>
        )}
        <SelectItem value="local">{t("ssh.local")}</SelectItem>
        {hosts.map((host) => (
          <SelectItem key={host.id} value={host.id}>
            {host.name}
          </SelectItem>
        ))}
      </SelectContent>
    </Select>
  );
}

export function MachineError({ error }: { error: unknown }) {
  return (
    <p
      role="alert"
      className="max-w-full whitespace-pre-wrap break-words text-sm text-destructive"
    >
      {extractErrorMessage(error) || String(error)}
    </p>
  );
}
