//! AppHost synchronous wrappers for training commands and queries (single-writer thread).

use delta_data::DeleteTrainingRunPreview;
use serde_json::Value;

use super::dto::*;
use crate::host::AppHost;
use crate::kline::KlineViewport;
use crate::service::ErrorEnvelope;

// ============================================================================
// 6. `AppHost` Synchronous Wrappers (Single-Writer Thread)
// ============================================================================

impl AppHost {
    pub fn list_fee_profile_versions(&self) -> Result<Vec<FeeProfileDto>, ErrorEnvelope> {
        self.execute(|svc| svc.list_fee_profile_versions())
    }

    pub fn get_fee_profile_version(&self, id: &str) -> Result<FeeProfileDto, ErrorEnvelope> {
        let id = id.to_string();
        self.execute(move |svc| svc.get_fee_profile_version(&id))
    }

    pub fn save_fee_profile_version(
        &self,
        draft: FeeProfileDraftDto,
    ) -> Result<FeeProfileDto, ErrorEnvelope> {
        self.execute(move |svc| svc.save_fee_profile_version(draft))
    }

    pub fn delete_unused_fee_profile_version(&self, id: &str) -> Result<(), ErrorEnvelope> {
        let id = id.to_string();
        self.execute(move |svc| svc.delete_unused_fee_profile_version(&id))
    }

    pub fn create_training_run(
        &self,
        req: CreateTrainingRunRequest,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        self.execute(move |svc| svc.create_training_run(req))
    }

    pub fn redraw_training_run(
        &self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.redraw_training_run(&run_id))
    }

    pub fn start_training_run(&self, run_id: &str) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.start_training_run(&run_id))
    }

    pub fn pause_training_run(&self, run_id: &str) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.pause_training_run(&run_id))
    }

    pub fn resume_training_run(
        &self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.resume_training_run(&run_id))
    }

    pub fn list_training_runs(&self) -> Result<Vec<TrainingRunSummaryDto>, ErrorEnvelope> {
        self.execute(|svc| svc.list_training_runs())
    }

    pub fn get_training_run_summary(
        &self,
        run_id: &str,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.get_training_run_summary(&run_id))
    }

    pub fn show_training_run(
        &self,
        run_id: &str,
    ) -> Result<TrainingWorkspaceSnapshotDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.show_training_run(&run_id))
    }

    pub fn get_training_workspace_snapshot(
        &self,
        run_id: &str,
        viewport_override: Option<KlineViewport>,
        max_pixel_columns: usize,
    ) -> Result<TrainingWorkspaceSnapshotDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| {
            svc.get_training_workspace_snapshot(&run_id, viewport_override, max_pixel_columns)
        })
    }

    pub fn preview_training_order(
        &self,
        run_id: &str,
        req: TrainingOrderRequestDto,
    ) -> Result<TrainingOrderPreviewDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.preview_training_order(&run_id, req))
    }

    pub fn submit_training_order(
        &self,
        run_id: &str,
        req: TrainingOrderRequestDto,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.submit_training_order(&run_id, req))
    }

    pub fn cancel_training_order(
        &self,
        run_id: &str,
        order_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        let order_id = order_id.to_string();
        self.execute(move |svc| svc.cancel_training_order(&run_id, &order_id))
    }

    pub fn advance_training_bar(
        &self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.advance_training_bar(&run_id))
    }

    pub fn finish_training_run(
        &self,
        run_id: &str,
    ) -> Result<TrainingStepReceiptDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.finish_training_run(&run_id))
    }

    pub fn reveal_training_run(&self, run_id: &str) -> Result<TrainingRevealDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.reveal_training_run(&run_id))
    }

    pub fn get_training_report(&self, run_id: &str) -> Result<TrainingReportDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.get_training_report(&run_id))
    }

    pub fn replay_training_run(
        &self,
        run_id: &str,
        mode: TrainingReplayMode,
    ) -> Result<TrainingReplayResultDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.replay_training_run(&run_id, mode))
    }

    pub fn retry_training_run(
        &self,
        original_run_id: &str,
        new_run_id: Option<String>,
    ) -> Result<TrainingRunSummaryDto, ErrorEnvelope> {
        let original_run_id = original_run_id.to_string();
        self.execute(move |svc| svc.retry_training_run(&original_run_id, new_run_id))
    }

    pub fn preview_delete_training_run(
        &self,
        run_id: &str,
    ) -> Result<DeleteTrainingRunPreview, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.preview_delete_training_run(&run_id))
    }

    pub fn delete_training_run(
        &self,
        run_id: &str,
    ) -> Result<DeleteTrainingRunPreview, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.delete_training_run(&run_id))
    }

    pub fn upsert_training_note(
        &self,
        run_id: &str,
        req: UpsertTrainingNoteRequest,
    ) -> Result<TrainingNoteDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.upsert_training_note(&run_id, req))
    }

    pub fn list_training_notes_dto(
        &self,
        run_id: &str,
    ) -> Result<Vec<TrainingNoteDto>, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.list_training_notes_dto(&run_id))
    }

    pub fn archive_training_notes_to_journal(
        &self,
        run_id: &str,
        note_ids: Vec<String>,
    ) -> Result<ArchiveTrainingNotesResultDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.archive_training_notes_to_journal(&run_id, &note_ids))
    }

    pub fn save_training_self_evaluation(
        &self,
        run_id: &str,
        decision_score: u8,
        plan_discipline_score: Option<u8>,
        summary: String,
    ) -> Result<UserSelfEvaluationDto, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| {
            svc.save_training_self_evaluation(
                &run_id,
                decision_score,
                plan_discipline_score,
                &summary,
            )
        })
    }

    pub fn get_training_self_evaluation(
        &self,
        run_id: &str,
    ) -> Result<Option<UserSelfEvaluationDto>, ErrorEnvelope> {
        let run_id = run_id.to_string();
        self.execute(move |svc| svc.get_training_self_evaluation(&run_id))
    }

    pub fn open_training_evidence(
        &self,
        run_id: &str,
        evidence_id: &str,
    ) -> Result<Value, ErrorEnvelope> {
        let run_id = run_id.to_string();
        let evidence_id = evidence_id.to_string();
        self.execute(move |svc| svc.open_training_evidence(&run_id, &evidence_id))
    }
}
