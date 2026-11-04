export type Mode = 'serve' | 'proxy';

export interface Config {
  name: string;
  mode: Mode;
  store: string;
  socket: string | null;
  tcp: string | null;
  upstream: string;
  blob_dir: string | null;
  max_message_size: number;
  log_level: string;
  local_cache_paths: string[];
}

export interface Summary {
  up_bytes: number;
  down_bytes: number;
  transfers: number;
  errors: number;
  kv_hit: number;
  kv_miss: number;
}

export interface ServiceStatus {
  id: string;
  config: Config;
  running: boolean;
  /** Unix ms. */
  started_at: number | null;
  error: string | null;
  /** Transitional state while the daemon starts/stops/clears this service: "starting" | "stopping" | "clearing". */
  busy: string | null;
  summary: Summary;
}

export interface Status {
  services: ServiceStatus[];
  records_dir: string;
  platform: string;
  unix_socket_supported: boolean;
  summary: Summary;
}

export interface PayloadRef {
  id: string;
  label: string;
  bytes: number;
}

export type Outcome = 'success' | 'hit' | 'miss' | 'error';

export interface Transfer {
  id: number;
  started_at: number;
  duration_us: number;
  peer: string;
  method: string;
  direction: 'up' | 'down';
  object_id: string | null;
  outcome: Outcome;
  bytes: number;
  grpc_status: string;
  error: string | null;
  request: Record<string, unknown> | null;
  response: Record<string, unknown> | null;
  payloads: PayloadRef[];
  service_id: string | null;
}

export interface AppEvent {
  id: number;
  time: number;
  level: string;
  kind: string;
  message: string;
  peer: string | null;
  transfer_id: number | null;
  service_id: string | null;
  fields: unknown;
}

export interface Page<T> {
  items: T[];
  total: number;
}

export interface PayloadChunk {
  id: string;
  offset: number;
  total: number;
  hex: string;
  text: string;
}

export interface UsagePart {
  bytes: number;
  files: number;
}

export interface Usage {
  objects: UsagePart;
  kv: UsagePart;
  tmp: UsagePart;
  total_bytes: number;
  total_files: number;
  records: UsagePart;
  disk_available: number;
  disk_total: number;
  scanned_at: number;
}

export interface ClearPlan {
  store: string;
  paths: string[];
  local_paths: string[];
  estimated_bytes: number;
  running: boolean;
  plan_id: string;
}

export interface ClearResult {
  reclaimed_bytes: number;
  removed_files: number;
  restarted: boolean;
  usage: Usage;
}

export type PageId = 'service' | 'transfers' | 'events' | 'cache';
