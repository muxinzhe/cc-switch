import { Channel, invoke } from "@/lib/machineIpc";
import { useQueries, useQuery } from "@tanstack/react-query";
import type {
  ContentRef,
  ImageRef,
  SessionMeta,
  TranscriptChunk,
} from "@/types";
import type { BlockContent, BlockContentPage } from "@/lib/api/sessions";

export interface SshHost {
  id: string;
  name: string;
  destination: string;
  port?: number | null;
  identityFile?: string | null;
  enabled: boolean;
}

export type HostInput = Omit<SshHost, "id"> & { id?: string };

export type ConfigFileKey =
  | "claude"
  | "codex"
  | "geminiSettings"
  | "geminiEnv"
  | "openCode"
  | "openClaw"
  | "hermes"
  | "pi";

interface LocalConfigFile {
  path: string;
  contents: string;
}

interface RemoteConfigFile extends LocalConfigFile {
  revision: string;
  exists: boolean;
}

export const sshKeys = {
  all: ["ssh"] as const,
  hosts: ["ssh", "hosts"] as const,
  host: (id: string) => ["ssh", "host", id] as const,
};

export const sshApi = {
  hosts: () => invoke<SshHost[]>("ssh_hosts"),
  aliases: () => invoke<string[]>("ssh_aliases"),
  saveHost: (input: HostInput) => invoke<SshHost>("ssh_save_host", { input }),
  removeHost: (id: string) => invoke<void>("ssh_remove_host", { id }),
  connect: (hostId: string) => invoke("ssh_connect", { hostId }),
  sessions: (hostId: string) =>
    invoke<SessionMeta[]>("ssh_sessions", { hostId }),
  streamMessages: (
    hostId: string,
    session: SessionMeta,
    onChunk: (chunk: TranscriptChunk) => void,
  ) => {
    const channel = new Channel<TranscriptChunk>();
    channel.onmessage = onChunk;
    return invoke<void>("ssh_stream_session_messages", {
      hostId,
      app: session.providerId,
      sourcePath: session.sourcePath,
      onChunk: channel,
    });
  },
  getBlockContent: (
    hostId: string,
    app: string,
    sourcePath: string,
    contentRef: ContentRef,
    page: BlockContentPage,
  ) =>
    invoke<BlockContent>("ssh_session_block_content", {
      hostId,
      app,
      sourcePath,
      contentRef,
      ...page,
    }),
  getImage: (
    hostId: string,
    app: string,
    sourcePath: string,
    image: ImageRef,
  ) =>
    invoke<ArrayBuffer>("ssh_session_image", {
      hostId,
      app,
      sourcePath,
      image,
    }),
  usageSnapshot: (
    hostIds: string[],
    includeLocal: boolean,
    forceScan?: boolean,
  ) =>
    invoke<{ id: string; failures: string[] }>("ssh_usage_snapshot", {
      hostIds,
      includeLocal,
      ...(forceScan ? { forceScan } : {}),
    }),
  readConfigFile: (hostId: string, key: ConfigFileKey) =>
    invoke<RemoteConfigFile>("ssh_read_config_file", { hostId, key }),
  readLocalConfigFile: (key: ConfigFileKey) =>
    invoke<LocalConfigFile>("ssh_read_local_config_file", { key }),
  writeConfigFile: (
    hostId: string,
    key: ConfigFileKey,
    draft: { contents: string; revision: string },
  ) =>
    invoke<{ backupPath: string | null }>("ssh_write_config_file", {
      hostId,
      key,
      draft,
    }),
};

export function useSshHosts(enabled = true) {
  return useQuery({
    queryKey: sshKeys.hosts,
    queryFn: sshApi.hosts,
    enabled,
    staleTime: Infinity,
    retry: false,
  });
}

function hostQuery<T>(
  id: string,
  key: readonly unknown[],
  query: (hostId: string) => Promise<T>,
) {
  return {
    queryKey: [...sshKeys.host(id), ...key],
    queryFn: () => query(id),
    staleTime: 60_000,
    retry: false,
    refetchOnWindowFocus: false,
  };
}

export function useHostQueries<T>(
  hosts: SshHost[],
  key: readonly unknown[],
  query: (hostId: string) => Promise<T>,
) {
  return useQueries({
    queries: hosts.map((host) => hostQuery(host.id, key, query)),
  });
}

export function useSshSessions(hosts: SshHost[]) {
  return useQueries({
    queries: hosts.map(({ id }) =>
      hostQuery(id, ["sessions"], sshApi.sessions),
    ),
    combine: (results) => ({
      sessions: results.flatMap(({ data }, index) =>
        (data ?? []).map(
          (session): SessionMeta => ({
            ...session,
            resumeCommand: undefined,
            machineId: hosts[index].id,
            machineName: hosts[index].name,
          }),
        ),
      ),
      pending: results.some((result) => result.isLoading),
      failures: results.flatMap(({ error }, index) =>
        error ? [{ host: hosts[index], error }] : [],
      ),
      refetch: () => Promise.all(results.map((result) => result.refetch())),
    }),
  });
}
