import type { RefObject } from "react";
import { useCallback, useMemo } from "react";

import { Spinner } from "@hypr/ui/components/ui/spinner";

import { useRegenerateTranscript } from "./actions";
import { TranscriptViewer } from "./renderer";
import { BatchState } from "./screens/batch";
import { TranscriptEmptyState } from "./screens/empty";
import { TranscriptListeningState } from "./screens/listening";
import { useTranscriptScreen } from "./state";

import type { BatchSegmentResult } from "~/store/zustand/listener/batch";
import { useListener } from "~/stt/contexts";
import { useUploadFile } from "~/stt/useUploadFile";

export function Transcript({
  sessionId,
  scrollRef,
}: {
  sessionId: string;
  scrollRef: RefObject<HTMLDivElement | null>;
}) {
  const screen = useTranscriptScreen({ sessionId });
  const { uploadAudio, uploadTranscript } = useUploadFile(sessionId);
  const regenerateTranscript = useRegenerateTranscript(sessionId);
  const stopTranscription = useListener((state) => state.stopTranscription);
  const stopRecording = useListener((state) => state.stop);
  const handleStopTranscription = useCallback(() => {
    void stopTranscription(sessionId);
  }, [sessionId, stopTranscription]);

  const runningBatch = screen.kind === "running_batch" ? screen : null;
  const hasSegments =
    runningBatch && Object.keys(runningBatch.segmentResponses).length > 0;
  const fallbackSegments =
    screen.kind === "batch_fallback"
      ? Object.keys(screen.segmentResponses).length > 0
      : false;

  return (
    <div className="relative flex h-full flex-col overflow-hidden">
      {screen.kind === "batch_fallback" && fallbackSegments && (
        <>
          <CompactProgress
            percentage={undefined}
            phase={undefined}
            segmentCount={Object.keys(screen.segmentResponses).length}
            onStop={stopRecording}
            recording
          />
          <SegmentPreview segmentResponses={screen.segmentResponses} />
        </>
      )}
      {runningBatch && hasSegments && (
        <>
          <CompactProgress
            percentage={runningBatch.percentage}
            phase={runningBatch.phase}
            segmentCount={Object.keys(runningBatch.segmentResponses).length}
            onStop={
              runningBatch.phase === "importing"
                ? undefined
                : handleStopTranscription
            }
          />
          <SegmentPreview segmentResponses={runningBatch.segmentResponses} />
        </>
      )}
      {runningBatch && !hasSegments && (
        <TranscriptEmptyState
          isBatching
          percentage={runningBatch.percentage}
          phase={runningBatch.phase}
          onStopTranscription={
            runningBatch.phase === "importing"
              ? undefined
              : handleStopTranscription
          }
        />
      )}
      {screen.kind === "batch_fallback" && !fallbackSegments && (
        <BatchState
          requestedLiveTranscription={screen.requestedLiveTranscription}
          error={screen.error}
        />
      )}
      {screen.kind === "listening" && (
        <TranscriptListeningState status={screen.status} />
      )}
      {screen.kind === "empty" && (
        <TranscriptEmptyState
          isBatching={false}
          hasAudio={screen.hasAudio}
          error={screen.error}
          onRetranscribe={regenerateTranscript}
          onUploadAudio={uploadAudio}
          onUploadTranscript={uploadTranscript}
        />
      )}
      {screen.kind === "ready" && (
        <>
          {screen.isFinalizing ? <FinalizingTranscriptBanner /> : null}
          <TranscriptViewer
            transcriptIds={screen.transcriptIds}
            liveSegments={screen.liveSegments}
            currentActive={screen.currentActive}
            scrollRef={scrollRef}
          />
        </>
      )}
    </div>
  );
}

function CompactProgress({
  percentage,
  phase,
  segmentCount,
  onStop,
  recording,
}: {
  percentage?: number;
  phase?: "importing" | "transcribing";
  segmentCount: number;
  onStop?: () => void;
  recording?: boolean;
}) {
  const pct =
    typeof percentage === "number" ? Math.round(percentage * 100) : null;

  return (
    <div className="flex items-center gap-3 border-b px-4 py-2">
      <Spinner size={14} />
      <div className="text-muted-foreground flex-1 text-xs">
        {recording
          ? "Recording... transcription in progress"
          : phase === "importing"
            ? "Importing audio..."
            : `Transcribing... ${pct !== null ? `${pct}%` : ""}`}
        {" · "}
        {segmentCount} segment{segmentCount > 1 ? "s" : ""} done
      </div>
      {onStop && (
        <button
          onClick={onStop}
          className="text-muted-foreground hover:text-foreground text-xs underline"
        >
          Stop
        </button>
      )}
    </div>
  );
}

function SegmentPreview({
  segmentResponses,
}: {
  segmentResponses: Record<number, BatchSegmentResult>;
}) {
  const entries = useMemo(
    () =>
      Object.entries(segmentResponses)
        .map(([index, { response, globalStartMs }]) => ({
          index: Number(index),
          globalStartMs,
          transcript:
            response.results.channels[0]?.alternatives[0]?.transcript ?? "",
          lastWordEndMs: getLastWordEndMs(response),
        }))
        .sort((a, b) => a.index - b.index),
    [segmentResponses],
  );

  if (entries.length === 0) {
    return null;
  }

  return (
    <div className="scrollbar-thin flex-1 space-y-4 overflow-y-auto px-4 pt-3 pb-4">
      {entries.map(({ index, globalStartMs, transcript, lastWordEndMs }) => (
        <div key={index}>
          <p className="text-muted-foreground text-xs">
            [{formatSegmentTime(globalStartMs)} –{" "}
            {formatSegmentTime(
              lastWordEndMs !== null ? globalStartMs + lastWordEndMs : null,
            )}
            ]
          </p>
          <p className="text-foreground text-sm leading-relaxed whitespace-pre-wrap">
            {transcript || "(no speech detected)"}
          </p>
        </div>
      ))}
    </div>
  );
}

function getLastWordEndMs(
  response: import("@hypr/plugin-transcription").BatchResponse,
): number | null {
  const words = response.results.channels[0]?.alternatives[0]?.words;
  const lastWord = words?.[words.length - 1];
  return typeof lastWord?.end === "number" ? lastWord.end * 1000 : null;
}

function formatSegmentTime(ms: number | null): string {
  if (ms === null) return "?";
  const totalSeconds = Math.max(0, Math.floor(ms / 1000));
  const mins = Math.floor(totalSeconds / 60);
  const secs = totalSeconds % 60;
  return `${mins}:${secs.toString().padStart(2, "0")}`;
}

function FinalizingTranscriptBanner() {
  return (
    <div className="bg-background/95 pointer-events-none absolute top-3 left-1/2 z-10 flex -translate-x-1/2 items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium shadow-sm">
      <Spinner size={14} />
      <span>Finalizing transcript...</span>
    </div>
  );
}
