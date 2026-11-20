import { createContext, useContext, type PropsWithChildren } from "react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import { sshApi } from "./api";
import { MachineError, useMachineScope } from "./MachineScope";

export const LOCAL_USAGE_SNAPSHOT_KEY = ["ssh", "usage-snapshot", true, []];

export function useUsageMachineSource(
  machines: ReturnType<typeof useMachineScope>,
  refreshInterval: number | false,
  { enabled = true, forceSyncOnRefresh = false } = {},
) {
  const queryClient = useQueryClient();
  const hostIds = machines.remote.map(({ id }) => id).sort();
  const includeLocal = machines.includeLocal;
  const scoped = includeLocal || hostIds.length > 0;
  const queryKey =
    includeLocal && hostIds.length === 0
      ? LOCAL_USAGE_SNAPSHOT_KEY
      : ["ssh", "usage-snapshot", includeLocal, hostIds];
  const snapshot = useQuery({
    queryKey,
    queryFn: () => sshApi.usageSnapshot(hostIds, includeLocal),
    enabled: scoped && enabled && !machines.pending,
    refetchInterval: refreshInterval,
    gcTime: 0,
    retry: false,
    refetchOnWindowFocus: includeLocal && hostIds.length === 0,
  });

  return {
    scopeKey: scoped ? JSON.stringify([includeLocal, hostIds]) : undefined,
    snapshotId: snapshot.data?.id,
    ready: !machines.pending && (!scoped || snapshot.data != null),
    includeLocal,
    pending:
      machines.pending || (scoped && snapshot.isFetching && !snapshot.data),
    error: machines.error ?? snapshot.error,
    failures: snapshot.data?.failures ?? [],
    updatedAt: snapshot.dataUpdatedAt,
    hosts: machines.hosts,
    refresh: scoped
      ? async () => {
          if (forceSyncOnRefresh) {
            // Drop any in-flight cache read so the manual scan replaces the shared snapshot.
            await queryClient.cancelQueries({ queryKey, exact: true });
            await queryClient.fetchQuery({
              queryKey,
              queryFn: () => sshApi.usageSnapshot(hostIds, includeLocal, true),
              retry: false,
            });
            return;
          }
          await snapshot.refetch({ throwOnError: true, cancelRefetch: false });
        }
      : undefined,
  };
}

const UsageMachineContext = createContext<
  ReturnType<typeof useUsageMachineSource> | undefined
>(undefined);

export function UsageMachineProvider({
  value,
  children,
}: PropsWithChildren<{ value: ReturnType<typeof useUsageMachineSource> }>) {
  return (
    <UsageMachineContext.Provider value={value}>
      {children}
    </UsageMachineContext.Provider>
  );
}

export function useUsageMachine() {
  return useContext(UsageMachineContext);
}

export function UsageMachineNotice({
  source,
}: {
  source: ReturnType<typeof useUsageMachineSource>;
}) {
  const { t } = useTranslation();
  if (!source.pending && !source.error && source.failures.length === 0) {
    return null;
  }
  return (
    <div className="space-y-1 px-6 py-2">
      {source.pending && (
        <p role="status" className="text-caption text-fg-2">
          {t("common.loading")}
        </p>
      )}
      {source.error && <MachineError error={source.error} />}
      {source.failures.length > 0 && (
        <p className="text-caption text-fg-2">{t("ssh.partialUsage")}</p>
      )}
      {source.failures.map((failure) => (
        <MachineError key={failure} error={failure} />
      ))}
    </div>
  );
}
