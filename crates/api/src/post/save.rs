use actix_web::web::{Data, Json};
use lemmy_api_common::{
  context::LemmyContext,
  post::{PostResponse, SavePost},
};
use lemmy_db_schema::{
  source::post::{PostRead, PostSaved, PostSavedForm},
  traits::Saveable,
};
use lemmy_db_views::structs::{LocalUserView, PostView};
use lemmy_utils::error::{LemmyErrorExt, LemmyErrorType, LemmyResult};

#[tracing::instrument(skip(context))]
pub async fn save_post(
  data: Json<SavePost>,
  context: Data<LemmyContext>,
  local_user_view: LocalUserView,
) -> LemmyResult<Json<PostResponse>> {
  let post_saved_form = PostSavedForm {
    post_id: data.post_id,
    person_id: local_user_view.person.id,
  };

  if data.save {
    PostSaved::save(&mut context.pool(), &post_saved_form)
      .await
      .with_lemmy_type(LemmyErrorType::CouldntSavePost)?;
  } else {
    PostSaved::unsave(&mut context.pool(), &post_saved_form)
      .await
      .with_lemmy_type(LemmyErrorType::CouldntSavePost)?;
  }

  let post_id = data.post_id;
  let person_id = local_user_view.person.id;
  let post_view = PostView::read(
    &mut context.pool(),
    post_id,
    Some(&local_user_view.local_user),
    false,
  )
  .await?;

  PostRead::mark_as_read(&mut context.pool(), post_id, person_id).await?;

  Ok(Json(PostResponse { post_view }))
}
