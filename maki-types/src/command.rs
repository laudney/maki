pub const ARGUMENTS_PLACEHOLDER: &str = "$ARGUMENTS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandScope {
    Project,
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommand {
    pub name: String,
    pub description: String,
    pub content: String,
    pub scope: CommandScope,
    pub accepts_args: bool,
}

impl CustomCommand {
    pub fn display_name(&self) -> String {
        let prefix = match self.scope {
            CommandScope::Project => "/project",
            CommandScope::User => "/user",
        };
        format!("{prefix}:{}", self.name)
    }

    pub fn has_args(&self) -> bool {
        self.accepts_args
    }

    pub fn render(&self, args: &str) -> String {
        self.content.replace(ARGUMENTS_PLACEHOLDER, args)
    }
}
