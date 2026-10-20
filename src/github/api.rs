use anyhow::Result;
use octocrab::Octocrab;
use tracing::info;

pub struct GitHubApiClient {
    octocrab: Octocrab,
    repo_owner: String,
    repo_name: String,
}

impl GitHubApiClient {
    pub fn new(token: String, repo_owner: String, repo_name: String) -> Result<Self> {
        let octocrab = Octocrab::builder()
            .personal_token(token)
            .build()?;

        Ok(Self {
            octocrab,
            repo_owner,
            repo_name,
        })
    }

    pub async fn get_pr_details(&self, pr_number: u64) -> Result<octocrab::models::pulls::PullRequest> {
        let pr = self
            .octocrab
            .pulls(&self.repo_owner, &self.repo_name)
            .get(pr_number)
            .await?;

        Ok(pr)
    }

    pub async fn add_pr_comment(&self, pr_number: u64, comment: &str) -> Result<()> {
        self.octocrab
            .issues(&self.repo_owner, &self.repo_name)
            .create_comment(pr_number, comment)
            .await?;

        info!("Added comment to PR #{}", pr_number);
        Ok(())
    }

    pub async fn add_pr_label(&self, pr_number: u64, label: &str) -> Result<()> {
        self.octocrab
            .issues(&self.repo_owner, &self.repo_name)
            .add_labels(pr_number, &[label])
            .await?;

        info!("Added label '{}' to PR #{}", label, pr_number);
        Ok(())
    }

    pub async fn get_user_contributions(&self, username: &str) -> Result<Vec<octocrab::models::pulls::PullRequest>> {
        let prs = self
            .octocrab
            .pulls(&self.repo_owner, &self.repo_name)
            .list()
            .state(octocrab::params::State::All)
            .send()
            .await?;

        let user_prs: Vec<_> = prs
            .items
            .into_iter()
            .filter(|pr| pr.user.as_ref().map(|u| u.login == username).unwrap_or(false))
            .collect();

        Ok(user_prs)
    }
}
