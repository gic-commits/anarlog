import type { RefObject } from "react";
import { useCallback, useMemo } from "react";

import { Spinner } from "@hypr/ui/components/ui/spinner";

import { useRegenerateTranscript } from "./actions";
import { TranscriptViewer } from "./renderer";
import { BatchState } from "./screens/batch";
import { TranscriptEmptyState } from "./screens/empty";
import { TranscriptListeningState } from "./screens/listening";
import { useTranscriptScreen } from "./state";

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
  const handleStopTranscription = useCallback(() => {
    void stopTranscription(sessionId);
  }, [sessionId, stopTranscription]);

  const runningBatch = screen.kind === "running_batch" ? screen : null;
  const hasSegments =
    runningBatch && Object.keys(runningBatch.segmentResponses).length > 0;

  return (
    <div className="relative flex h-full flex-col overflow-hidden">
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
      {screen.kind === "batch_fallback" && (
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
}: {
  percentage?: number;
  phase?: "importing" | "transcribing";
  segmentCount: number;
  onStop?: () => void;
}) {
  const pct =
    typeof percentage === "number" ? Math.round(percentage * 100) : null;

  return (
    <div className="flex items-center gap-3 border-b px-4 py-2">
      <Spinner size={14} />
      <div className="text-muted-foreground flex-1 text-xs">
        {phase === "importing"
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
  segmentResponses: Record<
    number,
    import("@hypr/plugin-transcription").BatchResponse
  >;
}) {
  const entries = useMemo(
    () =>
      Object.entries(segmentResponses)
        .map(([index, response]) => ({
          index: Number(index),
          transcript:
            response.results.channels[0]?.alternatives[0]?.transcript ?? "",
        }))
        .sort((a, b) => a.index - b.index),
    [segmentResponses],
  );

  if (entries.length === 0) {
    return null;
  }

  return (
    <div className="scrollbar-thin flex-1 space-y-4 overflow-y-auto px-4 pt-3 pb-4">
      {entries.map(({ index, transcript }) => (
        <div key={index}>
          <p className="text-foreground text-sm leading-relaxed whitespace-pre-wrap">
            {transcript || "(no speech detected)"}
          </p>
        </div>
      ))}
    </div>
  );
}

function FinalizingTranscriptBanner() {
  return (
    <div className="bg-background/95 pointer-events-none absolute top-3 left-1/2 z-10 flex -translate-x-1/2 items-center gap-2 rounded-full border px-3 py-1.5 text-xs font-medium shadow-sm">
      <Spinner size={14} />
      <span>Finalizing transcript...</span>
    </div>
  );
}
