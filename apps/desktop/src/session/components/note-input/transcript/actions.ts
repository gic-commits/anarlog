import { useQuery } from "@tanstack/react-query";
import { useCallback } from "react";

import { commands as fsSyncCommands } from "@hypr/plugin-fs-sync";
import { commands as listenerCommands } from "@hypr/plugin-transcription";
import { sonnerToast } from "@hypr/ui/components/ui/toast";

import { getEnhancerService } from "~/services/enhancer";
import { useSession } from "~/session/queries";
import { id } from "~/shared/utils";
import { useListener } from "~/stt/contexts";
import { createTranscript } from "~/stt/queries";
import type { RuntimeSpeakerHint, WordLike } from "~/stt/segment";
import type { SpeakerHintWithId, WordWithId } from "~/stt/types";
import { isStoppedTranscriptionError, useRunBatch } from "~/stt/useRunBatch";
import { useSTTConnection } from "~/stt/useSTTConnection";

export type RegenMode = "total" | "progressive";

export function useContinuableBatchJob(sessionId: string) {
  return useQuery({
    queryKey: ["continuable-batch-job", sessionId],
    queryFn: async () => {
      const result = await listenerCommands.listProgressiveBatchJobs(sessionId);
      if (result.status === "error") {
        return false;
      }
      const jobs = Array.isArray(result.data) ? result.data : [];
      return jobs.some(
        (j: unknown) =>
          (j as { status?: string }).status === "interrupted" ||
          (j as { status?: string }).status === "partial",
      );
    },
    enabled: Boolean(sessionId),
    staleTime: 10_000,
  });
}

export function useRegenerateTranscript(sessionId: string, mode?: RegenMode) {
  const runBatch = useRunBatch(sessionId);
  const handleBatchFailed = useListener((state) => state.handleBatchFailed);

  return useCallback(async () => {
    console.log(
      "[DEBUG] regenerateTranscript: sessionId=%s mode=%s",
      sessionId,
      mode,
    );

    const result = await fsSyncCommands.audioPath(sessionId);
    if (result.status === "error") {
      sonnerToast.error("Recording not found. It may have been deleted.", {
        id: `transcript-regenerate-audio-missing-${sessionId}`,
      });
      return;
    }

    const audioPath = result.data;

    try {
      const forceProgressive =
        mode === "progressive" ? true : mode === "total" ? false : undefined;
      await runBatch(audioPath, { forceProgressive });
      await getEnhancerService()?.queueAutoEnhanceIfSummaryEmpty(sessionId);
    } catch (error) {
      if (isStoppedTranscriptionError(error)) {
        return;
      }
      const msg = error instanceof Error ? error.message : String(error);
      handleBatchFailed(sessionId, msg);
    }
  }, [handleBatchFailed, mode, runBatch, sessionId]);
}

export function useContinueTranscript(sessionId: string) {
  const session = useSession(sessionId);
  const { conn } = useSTTConnection();
  const handleBatchStarted = useListener((state) => state.handleBatchStarted);
  const handleBatchResponse = useListener((state) => state.handleBatchResponse);
  const handleBatchFailed = useListener((state) => state.handleBatchFailed);
  const setBatchPersist = useListener((state) => state.setBatchPersist);

  const persist = useCallback(
    (words: WordLike[], hints: RuntimeSpeakerHint[]) => {
      if (words.length === 0) {
        return;
      }

      const newWords: WordWithId[] = words.map((word) => ({
        id: id(),
        text: word.text,
        start_ms: word.start_ms,
        end_ms: word.end_ms,
        channel: word.channel,
        metadata: word.metadata ? JSON.stringify(word.metadata) : undefined,
      }));

      const newHints: SpeakerHintWithId[] = [];

      hints.forEach((hint) => {
        if (hint.data.type !== "provider_speaker_index") {
          return;
        }

        const wordId = newWords[hint.wordIndex]?.id;
        const word = words[hint.wordIndex];

        if (!wordId || !word) {
          return;
        }

        newHints.push({
          id: id(),
          word_id: wordId,
          type: "provider_speaker_index",
          value: JSON.stringify({
            provider: hint.data.provider ?? "",
            channel: hint.data.channel ?? word.channel,
            speaker_index: hint.data.speaker_index,
          }),
        });
      });

      createTranscript({
        id: id(),
        sessionId,
        ownerUserId: session?.user_id ?? "",
        createdAt: new Date().toISOString(),
        startedAt: Date.now(),
        memo: session?.raw_md ?? "",
        source: "batch_transcription",
        provider: "",
        model: "",
        words: newWords,
        speakerHints: newHints,
        replaceSession: true,
      });
    },
    [session, sessionId],
  );

  return useCallback(async () => {
    console.log("[DEBUG] continueTranscript: sessionId=%s", sessionId);

    const result = await fsSyncCommands.audioPath(sessionId);
    if (result.status === "error") {
      sonnerToast.error("Recording not found.", {
        id: `transcript-continue-audio-missing-${sessionId}`,
      });
      return;
    }

    const audioPath = result.data;
    const apiKey = conn?.apiKey ?? "";
    handleBatchStarted(sessionId, "transcribing");
    setBatchPersist(sessionId, persist);

    try {
      const output = await listenerCommands.continueProgressiveBatch(
        sessionId,
        audioPath,
        apiKey,
      );

      if (output.status === "error") {
        throw new Error(output.error);
      }

      handleBatchResponse(sessionId, output.data.response);
      await getEnhancerService()?.queueAutoEnhanceIfSummaryEmpty(sessionId);
    } catch (error) {
      if (isStoppedTranscriptionError(error)) {
        return;
      }
      const msg = error instanceof Error ? error.message : String(error);
      handleBatchFailed(sessionId, msg);
    }
  }, [
    conn,
    handleBatchFailed,
    handleBatchResponse,
    handleBatchStarted,
    persist,
    sessionId,
  ]);
}
