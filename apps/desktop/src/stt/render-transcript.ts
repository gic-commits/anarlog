import type { TranscriptSpeakerHint } from "@hypr/plugin-fs-sync";
import { commands as listenerCommands } from "@hypr/plugin-transcription";
import type {
  IdentityAssignment,
  RenderTranscriptHuman,
  RenderTranscriptInput,
  RenderTranscriptRequest,
  RenderedTranscriptSegment,
} from "@hypr/plugin-transcription";

import type { SegmentWord } from "~/stt/live-segment";
import type { TranscriptWordMetadata } from "~/stt/timing";

export type RenderedTranscriptSegmentWithWordMetadata = Omit<
  RenderedTranscriptSegment,
  "words"
> & {
  words: SegmentWord[];
};

export type TranscriptRow = {
  started_at?: number | null;
  words?: Array<{
    id?: string | null;
    text?: string | null;
    start_ms?: number | null;
    end_ms?: number | null;
    channel?: number | null;
    metadata?: unknown;
  }> | null;
  speaker_hints?: Array<
    TranscriptSpeakerHint | { word_id?: string; type?: string; value?: unknown }
  > | null;
};

export type RenderTranscriptRequestHumans = {
  selfHumanId?: string;
  humans: RenderTranscriptHuman[];
};

export async function renderTranscriptSegments(
  request: RenderTranscriptRequest,
): Promise<RenderedTranscriptSegmentWithWordMetadata[]> {
  const normalizedRequest = normalizeRenderTranscriptRequest(request);
  const metadataByWordId = collectWordMetadataById(normalizedRequest);
  const cacheKey = getRenderTranscriptRequestKey(normalizedRequest);

  const cached = renderSegmentCache.get(cacheKey);
  if (cached) {
    return cached;
  }

  const pending = listenerCommands
    .renderTranscriptSegments(normalizedRequest)
    .then((result) => {
      if (result.status === "error") {
        throw new Error(result.error);
      }

      return attachWordMetadata(result.data, metadataByWordId);
    })
    .catch((error) => {
      renderSegmentCache.delete(cacheKey);
      throw error;
    });

  renderSegmentCache.set(cacheKey, pending);
  return pending;
}

const renderSegmentCache = new Map<
  string,
  Promise<RenderedTranscriptSegmentWithWordMetadata[]>
>();

export function getRenderTranscriptRequestKey(
  request: RenderTranscriptRequest | null | undefined,
): string {
  if (!request) {
    return "empty";
  }

  let hash = 2_166_136_261;
  let wordCount = 0;
  let assignmentCount = 0;

  const writeString = (value: string) => {
    for (let index = 0; index < value.length; index += 1) {
      hash =
        (Math.imul(hash ^ value.charCodeAt(index), 16_777_619) >>> 0) + index;
    }
    hash = (Math.imul(hash ^ 31, 16_777_619) >>> 0) + value.length;
  };

  const writeScalar = (value: number | null | undefined) => {
    if (value == null) {
      writeString("n");
      return;
    }
    writeString(String(value));
  };

  writeString(request.self_human_id ?? "");

  for (const humanId of request.participant_human_ids) {
    writeString(humanId);
  }

  for (const human of request.humans) {
    writeString(human.human_id);
    writeString(human.name);
  }

  for (const transcript of request.transcripts) {
    writeScalar(transcript.started_at);
    wordCount += transcript.words.length;
    assignmentCount += transcript.assignments.length;

    for (const word of transcript.words) {
      writeString(word.id);
      writeString(word.text);
      writeScalar(word.start_ms);
      writeScalar(word.end_ms);
      writeScalar(word.channel);
      writeScalar(word.speaker_index);
      // Keep the expensive part out of the hash: only the scalar field that
      // actually changes segmentation needs to be sensitive.
      writeScalar(
        (word as { metadata?: { provider_segment_index?: number } }).metadata
          ?.provider_segment_index,
      );
    }

    for (const assignment of transcript.assignments) {
      writeString(assignment.human_id);
      writeString(JSON.stringify(assignment.scope));
    }
  }

  return [
    request.transcripts.length,
    wordCount,
    assignmentCount,
    hash.toString(36),
  ].join(":");
}

export function buildRenderTranscriptRequestFromRows(
  transcripts: TranscriptRow[],
  humans?: RenderTranscriptRequestHumans,
  participantHumanIds?: string[],
): RenderTranscriptRequest | null {
  return buildRenderTranscriptRequest(transcripts, humans, participantHumanIds);
}

export function collectAssignedHumanIdsFromTranscriptRows(
  transcripts: TranscriptRow[],
): string[] {
  const humanIds = new Set<string>();

  for (const transcript of transcripts) {
    for (const hint of transcript.speaker_hints ?? []) {
      if (hint.type !== "user_speaker_assignment") {
        continue;
      }

      const value = parseHintValue(hint.value);
      const humanId =
        value && typeof value === "object"
          ? (value as { human_id?: unknown }).human_id
          : undefined;

      if (typeof humanId === "string" && humanId) {
        humanIds.add(humanId);
      }
    }
  }

  return [...humanIds];
}

function buildRenderTranscriptRequest(
  transcripts: TranscriptRow[],
  humans?: RenderTranscriptRequestHumans,
  participantHumanIds?: string[],
): RenderTranscriptRequest | null {
  if (transcripts.length === 0) {
    return null;
  }

  const normalizedTranscripts: RenderTranscriptInput[] = [];

  for (const transcript of transcripts) {
    const words: RenderTranscriptInput["words"] = [];
    const assignments: IdentityAssignment[] = [];
    const wordIndexById = new Map<string, number>();

    for (const word of transcript.words ?? []) {
      if (
        typeof word.id !== "string" ||
        typeof word.text !== "string" ||
        typeof word.start_ms !== "number" ||
        typeof word.end_ms !== "number"
      ) {
        continue;
      }

      wordIndexById.set(word.id, words.length);
      const metadata = normalizeWordMetadata(word.metadata);
      const provider_segment_index =
        typeof metadata?.provider_segment_index === "number"
          ? metadata.provider_segment_index
          : undefined;
      const renderWord: RenderTranscriptInput["words"][number] & {
        metadata?: TranscriptWordMetadata;
      } = {
        id: word.id,
        text: word.text,
        start_ms: word.start_ms,
        end_ms: word.end_ms,
        channel: typeof word.channel === "number" ? word.channel : 0,
        speaker_index: null,
        ...(provider_segment_index !== undefined
          ? { provider_segment_index }
          : {}),
        ...(metadata ? { metadata } : {}),
      };
      words.push(renderWord);
    }

    for (const hint of transcript.speaker_hints ?? []) {
      if (hint.type !== "provider_speaker_index") {
        continue;
      }

      normalizeSpeakerHint(hint, words, wordIndexById);
    }

    for (const hint of transcript.speaker_hints ?? []) {
      if (hint.type === "provider_speaker_index") {
        continue;
      }

      const normalized = normalizeSpeakerHint(hint, words, wordIndexById);
      if (normalized) {
        assignments.push(normalized);
      }
    }

    if (words.length === 0) {
      continue;
    }

    normalizedTranscripts.push({
      started_at:
        typeof transcript.started_at === "number"
          ? transcript.started_at
          : null,
      words,
      assignments,
    });
  }

  if (normalizedTranscripts.length === 0) {
    return null;
  }

  return {
    transcripts: normalizedTranscripts,
    participant_human_ids: participantHumanIds ?? [],
    self_human_id: humans?.selfHumanId ?? null,
    humans: humans?.humans ?? [],
  };
}

function normalizeSpeakerHint(
  hint:
    | TranscriptSpeakerHint
    | { word_id?: string; type?: string; value?: unknown },
  words: RenderTranscriptInput["words"],
  wordIndexById: Map<string, number>,
): IdentityAssignment | null {
  if (typeof hint.word_id !== "string" || typeof hint.type !== "string") {
    return null;
  }

  const value = parseHintValue(hint.value);
  if (!value || typeof value !== "object") {
    return null;
  }

  const wordIndex = wordIndexById.get(hint.word_id);
  if (typeof wordIndex !== "number") {
    return null;
  }

  const word = words[wordIndex];
  if (!word) {
    return null;
  }

  if (
    hint.type === "provider_speaker_index" &&
    typeof (value as { speaker_index?: unknown }).speaker_index === "number"
  ) {
    word.speaker_index = (value as { speaker_index: number }).speaker_index;
    if (typeof (value as { channel?: unknown }).channel === "number") {
      word.channel = (value as { channel: number }).channel;
    }
    return null;
  }

  if (
    hint.type === "user_speaker_assignment" &&
    typeof (value as { human_id?: unknown }).human_id === "string"
  ) {
    const humanId = (value as { human_id: string }).human_id;
    if (
      (value as { scope?: unknown }).scope === "segment" &&
      Array.isArray((value as { word_ids?: unknown }).word_ids)
    ) {
      const wordIds = (value as { word_ids: unknown[] }).word_ids.filter(
        (wordId): wordId is string =>
          typeof wordId === "string" && wordId.length > 0,
      );
      if (wordIds.length > 0) {
        return {
          human_id: humanId,
          scope: {
            kind: "words",
            word_ids: wordIds,
          },
        };
      }
    }

    return word.speaker_index == null
      ? {
          human_id: humanId,
          scope: {
            kind: "channel",
            channel:
              word.channel === 0
                ? "DirectMic"
                : word.channel === 1
                  ? "RemoteParty"
                  : "MixedCapture",
          },
        }
      : {
          human_id: humanId,
          scope: {
            kind: "channel_speaker",
            channel:
              word.channel === 0
                ? "DirectMic"
                : word.channel === 1
                  ? "RemoteParty"
                  : "MixedCapture",
            speaker_index: word.speaker_index,
          },
        };
  }

  return null;
}

function parseHintValue(value: unknown): unknown {
  if (typeof value === "string") {
    try {
      return JSON.parse(value);
    } catch {
      return undefined;
    }
  }

  return value;
}

function normalizeRenderTranscriptRequest(
  request: RenderTranscriptRequest,
): RenderTranscriptRequest {
  return {
    ...request,
    transcripts: request.transcripts.map((transcript) => ({
      ...transcript,
      started_at: normalizeOptionalTranscriptMs(transcript.started_at),
      words: transcript.words.map((word) => ({
        ...word,
        start_ms: normalizeTranscriptMs(word.start_ms),
        end_ms: normalizeTranscriptMs(word.end_ms),
      })),
    })),
  };
}

function collectWordMetadataById(
  request: RenderTranscriptRequest,
): Map<string, TranscriptWordMetadata> {
  const metadataByWordId = new Map<string, TranscriptWordMetadata>();

  for (const transcript of request.transcripts) {
    for (const word of transcript.words) {
      const metadata = normalizeWordMetadata(
        (word as { metadata?: unknown }).metadata,
      );
      if (metadata) {
        metadataByWordId.set(word.id, metadata);
      }
    }
  }

  return metadataByWordId;
}

function attachWordMetadata(
  segments: RenderedTranscriptSegment[],
  metadataByWordId: Map<string, TranscriptWordMetadata>,
): RenderedTranscriptSegmentWithWordMetadata[] {
  if (metadataByWordId.size === 0) {
    return segments as RenderedTranscriptSegmentWithWordMetadata[];
  }

  return segments.map((segment) => ({
    ...segment,
    words: segment.words.map((word) =>
      attachMetadataToWord(word, metadataByWordId),
    ),
  }));
}

function attachMetadataToWord(
  word: RenderedTranscriptSegment["words"][number],
  metadataByWordId: Map<string, TranscriptWordMetadata>,
): SegmentWord {
  if (!word.id) {
    return word;
  }

  const metadata = metadataByWordId.get(word.id);
  return metadata ? { ...word, metadata } : word;
}

function normalizeWordMetadata(value: unknown): TranscriptWordMetadata | null {
  if (typeof value === "string") {
    try {
      return normalizeWordMetadata(JSON.parse(value));
    } catch {
      return null;
    }
  }

  return value && typeof value === "object" && !Array.isArray(value)
    ? (value as TranscriptWordMetadata)
    : null;
}

function normalizeTranscriptMs(value: number): number {
  return Number.isFinite(value) ? Math.round(value) : value;
}

function normalizeOptionalTranscriptMs(value: number | null): number | null {
  return typeof value === "number" ? normalizeTranscriptMs(value) : value;
}
