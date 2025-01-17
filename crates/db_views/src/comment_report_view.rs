use crate::structs::{CommentReportView, LocalUserView};
use diesel::{
  dsl::now,
  pg::Pg,
  result::Error,
  BoolExpressionMethods,
  ExpressionMethods,
  JoinOnDsl,
  NullableExpressionMethods,
  QueryDsl,
};
use diesel_async::RunQueryDsl;
use lemmy_db_schema::{
  aliases::{self, creator_community_actions},
  newtypes::{CommentId, CommentReportId, CommunityId, PersonId},
  schema::{
    comment,
    comment_actions,
    comment_aggregates,
    comment_report,
    community,
    community_actions,
    local_user,
    person,
    person_actions,
    post,
  },
  source::community::CommunityFollower,
  utils::{
    actions,
    actions_alias,
    functions::coalesce,
    get_conn,
    limit_and_offset,
    DbConn,
    DbPool,
    ListFn,
    Queries,
    ReadFn,
  },
};

fn queries<'a>() -> Queries<
  impl ReadFn<'a, CommentReportView, (CommentReportId, PersonId)>,
  impl ListFn<'a, CommentReportView, (CommentReportQuery, &'a LocalUserView)>,
> {
  let all_joins = |query: comment_report::BoxedQuery<'a, Pg>, my_person_id: PersonId| {
    query
      .inner_join(comment::table)
      .inner_join(post::table.on(comment::post_id.eq(post::id)))
      .inner_join(community::table.on(post::community_id.eq(community::id)))
      .inner_join(person::table.on(comment_report::creator_id.eq(person::id)))
      .inner_join(aliases::person1.on(comment::creator_id.eq(aliases::person1.field(person::id))))
      .inner_join(
        comment_aggregates::table.on(comment_report::comment_id.eq(comment_aggregates::comment_id)),
      )
      .left_join(actions(
        comment_actions::table,
        Some(my_person_id),
        comment_report::comment_id,
      ))
      .left_join(
        aliases::person2
          .on(comment_report::resolver_id.eq(aliases::person2.field(person::id).nullable())),
      )
      .left_join(actions_alias(
        creator_community_actions,
        comment::creator_id,
        post::community_id,
      ))
      .left_join(
        local_user::table.on(
          comment::creator_id
            .eq(local_user::person_id)
            .and(local_user::admin.eq(true)),
        ),
      )
      .left_join(actions(
        person_actions::table,
        Some(my_person_id),
        comment::creator_id,
      ))
      .left_join(actions(
        community_actions::table,
        Some(my_person_id),
        post::community_id,
      ))
      .select((
        comment_report::all_columns,
        comment::all_columns,
        post::all_columns,
        community::all_columns,
        person::all_columns,
        aliases::person1.fields(person::all_columns),
        comment_aggregates::all_columns,
        coalesce(
          creator_community_actions
            .field(community_actions::received_ban)
            .nullable()
            .is_not_null()
            .or(
              creator_community_actions
                .field(community_actions::ban_expires)
                .nullable()
                .gt(now),
            ),
          false,
        ),
        creator_community_actions
          .field(community_actions::became_moderator)
          .nullable()
          .is_not_null(),
        local_user::admin.nullable().is_not_null(),
        person_actions::blocked.nullable().is_not_null(),
        CommunityFollower::select_subscribed_type(),
        comment_actions::saved.nullable().is_not_null(),
        comment_actions::like_score.nullable(),
        aliases::person2.fields(person::all_columns).nullable(),
      ))
  };

  let read = move |mut conn: DbConn<'a>, (report_id, my_person_id): (CommentReportId, PersonId)| async move {
    all_joins(
      comment_report::table.find(report_id).into_boxed(),
      my_person_id,
    )
    .first(&mut conn)
    .await
  };

  let list = move |mut conn: DbConn<'a>,
                   (options, user): (CommentReportQuery, &'a LocalUserView)| async move {
    let mut query = all_joins(comment_report::table.into_boxed(), user.person.id);

    if let Some(community_id) = options.community_id {
      query = query.filter(post::community_id.eq(community_id));
    }

    if let Some(comment_id) = options.comment_id {
      query = query.filter(comment_report::comment_id.eq(comment_id));
    }

    // If viewing all reports, order by newest, but if viewing unresolved only, show the oldest
    // first (FIFO)
    if options.unresolved_only {
      query = query
        .filter(comment_report::resolved.eq(false))
        .order_by(comment_report::published.asc());
    } else {
      query = query.order_by(comment_report::published.desc());
    }

    let (limit, offset) = limit_and_offset(options.page, options.limit)?;

    query = query.limit(limit).offset(offset);

    // If its not an admin, get only the ones you mod
    if !user.local_user.admin {
      query = query.filter(community_actions::became_moderator.is_not_null());
    }

    query.load::<CommentReportView>(&mut conn).await
  };

  Queries::new(read, list)
}

impl CommentReportView {
  /// returns the CommentReportView for the provided report_id
  ///
  /// * `report_id` - the report id to obtain
  pub async fn read(
    pool: &mut DbPool<'_>,
    report_id: CommentReportId,
    my_person_id: PersonId,
  ) -> Result<Self, Error> {
    queries().read(pool, (report_id, my_person_id)).await
  }

  /// Returns the current unresolved comment report count for the communities you mod
  pub async fn get_report_count(
    pool: &mut DbPool<'_>,
    my_person_id: PersonId,
    admin: bool,
    community_id: Option<CommunityId>,
  ) -> Result<i64, Error> {
    use diesel::dsl::count;

    let conn = &mut get_conn(pool).await?;

    let mut query = comment_report::table
      .inner_join(comment::table)
      .inner_join(post::table.on(comment::post_id.eq(post::id)))
      .filter(comment_report::resolved.eq(false))
      .into_boxed();

    if let Some(community_id) = community_id {
      query = query.filter(post::community_id.eq(community_id))
    }

    // If its not an admin, get only the ones you mod
    if !admin {
      query
        .inner_join(
          community_actions::table.on(
            community_actions::community_id
              .eq(post::community_id)
              .and(community_actions::person_id.eq(my_person_id))
              .and(community_actions::became_moderator.is_not_null()),
          ),
        )
        .select(count(comment_report::id))
        .first::<i64>(conn)
        .await
    } else {
      query
        .select(count(comment_report::id))
        .first::<i64>(conn)
        .await
    }
  }
}

#[derive(Default)]
pub struct CommentReportQuery {
  pub community_id: Option<CommunityId>,
  pub comment_id: Option<CommentId>,
  pub page: Option<i64>,
  pub limit: Option<i64>,
  pub unresolved_only: bool,
}

impl CommentReportQuery {
  pub async fn list(
    self,
    pool: &mut DbPool<'_>,
    user: &LocalUserView,
  ) -> Result<Vec<CommentReportView>, Error> {
    queries().list(pool, (self, user)).await
  }
}

#[cfg(test)]
#[expect(clippy::indexing_slicing)]
mod tests {

  use crate::{
    comment_report_view::{CommentReportQuery, CommentReportView},
    structs::LocalUserView,
  };
  use lemmy_db_schema::{
    aggregates::structs::CommentAggregates,
    source::{
      comment::{Comment, CommentInsertForm},
      comment_report::{CommentReport, CommentReportForm},
      community::{Community, CommunityInsertForm, CommunityModerator, CommunityModeratorForm},
      instance::Instance,
      local_user::{LocalUser, LocalUserInsertForm},
      local_user_vote_display_mode::LocalUserVoteDisplayMode,
      person::{Person, PersonInsertForm},
      post::{Post, PostInsertForm},
    },
    traits::{Crud, Joinable, Reportable},
    utils::{build_db_pool_for_tests, RANK_DEFAULT},
    CommunityVisibility,
    SubscribedType,
  };
  use lemmy_utils::error::LemmyResult;
  use pretty_assertions::assert_eq;
  use serial_test::serial;

  #[tokio::test]
  #[serial]
  async fn test_crud() -> LemmyResult<()> {
    let pool = &build_db_pool_for_tests();
    let pool = &mut pool.into();

    let inserted_instance = Instance::read_or_create(pool, "my_domain.tld".to_string()).await?;

    let new_person = PersonInsertForm::test_form(inserted_instance.id, "timmy_crv");

    let inserted_timmy = Person::create(pool, &new_person).await?;

    let new_local_user = LocalUserInsertForm::test_form(inserted_timmy.id);
    let timmy_local_user = LocalUser::create(pool, &new_local_user, vec![]).await?;
    let timmy_view = LocalUserView {
      local_user: timmy_local_user,
      local_user_vote_display_mode: LocalUserVoteDisplayMode::default(),
      person: inserted_timmy.clone(),
      counts: Default::default(),
    };

    let new_person_2 = PersonInsertForm::test_form(inserted_instance.id, "sara_crv");

    let inserted_sara = Person::create(pool, &new_person_2).await?;

    // Add a third person, since new ppl can only report something once.
    let new_person_3 = PersonInsertForm::test_form(inserted_instance.id, "jessica_crv");

    let inserted_jessica = Person::create(pool, &new_person_3).await?;

    let new_community = CommunityInsertForm::new(
      inserted_instance.id,
      "test community crv".to_string(),
      "nada".to_owned(),
      "pubkey".to_string(),
    );
    let inserted_community = Community::create(pool, &new_community).await?;

    // Make timmy a mod
    let timmy_moderator_form = CommunityModeratorForm {
      community_id: inserted_community.id,
      person_id: inserted_timmy.id,
    };

    let _inserted_moderator = CommunityModerator::join(pool, &timmy_moderator_form).await?;

    let new_post = PostInsertForm::new(
      "A test post crv".into(),
      inserted_timmy.id,
      inserted_community.id,
    );

    let inserted_post = Post::create(pool, &new_post).await?;

    let comment_form = CommentInsertForm::new(
      inserted_timmy.id,
      inserted_post.id,
      "A test comment 32".into(),
    );
    let inserted_comment = Comment::create(pool, &comment_form, None).await?;

    // sara reports
    let sara_report_form = CommentReportForm {
      creator_id: inserted_sara.id,
      comment_id: inserted_comment.id,
      original_comment_text: "this was it at time of creation".into(),
      reason: "from sara".into(),
    };

    let inserted_sara_report = CommentReport::report(pool, &sara_report_form).await?;

    // jessica reports
    let jessica_report_form = CommentReportForm {
      creator_id: inserted_jessica.id,
      comment_id: inserted_comment.id,
      original_comment_text: "this was it at time of creation".into(),
      reason: "from jessica".into(),
    };

    let inserted_jessica_report = CommentReport::report(pool, &jessica_report_form).await?;

    let agg = CommentAggregates::read(pool, inserted_comment.id).await?;

    let read_jessica_report_view =
      CommentReportView::read(pool, inserted_jessica_report.id, inserted_timmy.id).await?;
    let expected_jessica_report_view = CommentReportView {
      comment_report: inserted_jessica_report.clone(),
      comment: inserted_comment.clone(),
      post: inserted_post,
      creator_is_moderator: true,
      creator_is_admin: false,
      creator_blocked: false,
      subscribed: SubscribedType::NotSubscribed,
      saved: false,
      community: Community {
        id: inserted_community.id,
        name: inserted_community.name,
        icon: None,
        removed: false,
        deleted: false,
        nsfw: false,
        actor_id: inserted_community.actor_id.clone(),
        local: true,
        title: inserted_community.title,
        sidebar: None,
        description: None,
        updated: None,
        banner: None,
        hidden: false,
        posting_restricted_to_mods: false,
        published: inserted_community.published,
        private_key: inserted_community.private_key,
        public_key: inserted_community.public_key,
        last_refreshed_at: inserted_community.last_refreshed_at,
        followers_url: inserted_community.followers_url,
        inbox_url: inserted_community.inbox_url,
        moderators_url: inserted_community.moderators_url,
        featured_url: inserted_community.featured_url,
        instance_id: inserted_instance.id,
        visibility: CommunityVisibility::Public,
      },
      creator: Person {
        id: inserted_jessica.id,
        name: inserted_jessica.name,
        display_name: None,
        published: inserted_jessica.published,
        avatar: None,
        actor_id: inserted_jessica.actor_id.clone(),
        local: true,
        banned: false,
        deleted: false,
        bot_account: false,
        bio: None,
        banner: None,
        updated: None,
        inbox_url: inserted_jessica.inbox_url.clone(),
        matrix_user_id: None,
        ban_expires: None,
        instance_id: inserted_instance.id,
        private_key: inserted_jessica.private_key,
        public_key: inserted_jessica.public_key,
        last_refreshed_at: inserted_jessica.last_refreshed_at,
      },
      comment_creator: Person {
        id: inserted_timmy.id,
        name: inserted_timmy.name.clone(),
        display_name: None,
        published: inserted_timmy.published,
        avatar: None,
        actor_id: inserted_timmy.actor_id.clone(),
        local: true,
        banned: false,
        deleted: false,
        bot_account: false,
        bio: None,
        banner: None,
        updated: None,
        inbox_url: inserted_timmy.inbox_url.clone(),
        matrix_user_id: None,
        ban_expires: None,
        instance_id: inserted_instance.id,
        private_key: inserted_timmy.private_key.clone(),
        public_key: inserted_timmy.public_key.clone(),
        last_refreshed_at: inserted_timmy.last_refreshed_at,
      },
      creator_banned_from_community: false,
      counts: CommentAggregates {
        comment_id: inserted_comment.id,
        score: 0,
        upvotes: 0,
        downvotes: 0,
        published: agg.published,
        child_count: 0,
        hot_rank: RANK_DEFAULT,
        controversy_rank: 0.0,
      },
      my_vote: None,
      resolver: None,
    };

    assert_eq!(read_jessica_report_view, expected_jessica_report_view);

    let mut expected_sara_report_view = expected_jessica_report_view.clone();
    expected_sara_report_view.comment_report = inserted_sara_report;
    expected_sara_report_view.creator = Person {
      id: inserted_sara.id,
      name: inserted_sara.name,
      display_name: None,
      published: inserted_sara.published,
      avatar: None,
      actor_id: inserted_sara.actor_id.clone(),
      local: true,
      banned: false,
      deleted: false,
      bot_account: false,
      bio: None,
      banner: None,
      updated: None,
      inbox_url: inserted_sara.inbox_url.clone(),
      matrix_user_id: None,
      ban_expires: None,
      instance_id: inserted_instance.id,
      private_key: inserted_sara.private_key,
      public_key: inserted_sara.public_key,
      last_refreshed_at: inserted_sara.last_refreshed_at,
    };

    // Do a batch read of timmys reports
    let reports = CommentReportQuery::default()
      .list(pool, &timmy_view)
      .await?;

    assert_eq!(
      reports,
      [
        expected_jessica_report_view.clone(),
        expected_sara_report_view.clone(),
      ]
    );

    // Make sure the counts are correct
    let report_count =
      CommentReportView::get_report_count(pool, inserted_timmy.id, false, None).await?;
    assert_eq!(2, report_count);

    // Try to resolve the report
    CommentReport::resolve(pool, inserted_jessica_report.id, inserted_timmy.id).await?;
    let read_jessica_report_view_after_resolve =
      CommentReportView::read(pool, inserted_jessica_report.id, inserted_timmy.id).await?;

    let mut expected_jessica_report_view_after_resolve = expected_jessica_report_view;
    expected_jessica_report_view_after_resolve
      .comment_report
      .resolved = true;
    expected_jessica_report_view_after_resolve
      .comment_report
      .resolver_id = Some(inserted_timmy.id);
    expected_jessica_report_view_after_resolve
      .comment_report
      .updated = read_jessica_report_view_after_resolve
      .comment_report
      .updated;
    expected_jessica_report_view_after_resolve.resolver = Some(Person {
      id: inserted_timmy.id,
      name: inserted_timmy.name.clone(),
      display_name: None,
      published: inserted_timmy.published,
      avatar: None,
      actor_id: inserted_timmy.actor_id.clone(),
      local: true,
      banned: false,
      deleted: false,
      bot_account: false,
      bio: None,
      banner: None,
      updated: None,
      inbox_url: inserted_timmy.inbox_url.clone(),
      private_key: inserted_timmy.private_key.clone(),
      public_key: inserted_timmy.public_key.clone(),
      last_refreshed_at: inserted_timmy.last_refreshed_at,
      matrix_user_id: None,
      ban_expires: None,
      instance_id: inserted_instance.id,
    });

    assert_eq!(
      read_jessica_report_view_after_resolve,
      expected_jessica_report_view_after_resolve
    );

    // Do a batch read of timmys reports
    // It should only show saras, which is unresolved
    let reports_after_resolve = CommentReportQuery {
      unresolved_only: (true),
      ..Default::default()
    }
    .list(pool, &timmy_view)
    .await?;
    assert_eq!(reports_after_resolve[0], expected_sara_report_view);
    assert_eq!(reports_after_resolve.len(), 1);

    // Make sure the counts are correct
    let report_count_after_resolved =
      CommentReportView::get_report_count(pool, inserted_timmy.id, false, None).await?;
    assert_eq!(1, report_count_after_resolved);

    Person::delete(pool, inserted_timmy.id).await?;
    Person::delete(pool, inserted_sara.id).await?;
    Person::delete(pool, inserted_jessica.id).await?;
    Community::delete(pool, inserted_community.id).await?;
    Instance::delete(pool, inserted_instance.id).await?;

    Ok(())
  }
}
