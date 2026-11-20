# Contributing to MSync (MeritSync)

Thank you for your interest in contributing to MSync! This document provides guidelines and instructions for contributing to our merit-based open source community.

## 🎯 Our Merit-Based Approach

MSync operates on a merit-based system where contributions are automatically evaluated and rewarded. Your Discord roles will automatically update based on your contribution quality and volume.

## 📋 Prerequisites

Before contributing, ensure you have:
- A GitHub account
- Basic knowledge of Rust and Discord bots
- Familiarity with Git and GitHub workflows
- Joined our Discord server and linked your GitHub account

## 🔄 Contribution Process

### 1. Find an Issue
- Check our [issue tracker](https://github.com/yourusername/msync/issues)
- Look for issues labeled `good first issue` or `help wanted`
- Comment on the issue to claim it

### 2. Fork and Clone
```bash
# Fork the repository on GitHub, then:
git clone https://github.com/YOUR_USERNAME/msync.git
cd msync
git remote add upstream https://github.com/ORIGINAL_OWNER/msync.git
```

### 3. Create a Branch
```bash
git checkout -b feature/your-feature-name
# or
git checkout -b fix/issue-number-description
```

### 4. Make Your Changes
- Follow our code style guidelines (see below)
- Write tests for new functionality
- Update documentation as needed
- Ensure all tests pass: `cargo test`

### 5. Commit Your Changes
```bash
# Use conventional commits format
git commit -m "feat: add new merit calculation algorithm"
# or
git commit -m "fix: resolve Discord role sync issue"
```

Commit types:
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `style`: Code style changes (formatting, etc.)
- `refactor`: Code refactoring
- `test`: Test additions or modifications
- `chore`: Maintenance tasks

### 6. Push and Create PR
```bash
git push origin feature/your-feature-name
```

Then create a Pull Request on GitHub with:
- Clear title and description
- Reference to related issues
- Screenshots/demos if applicable

## 📏 Code Style Guidelines

### Rust Style
- Follow standard Rust conventions
- Use `rustfmt` for formatting: `cargo fmt`
- Ensure `clippy` passes: `cargo clippy`
- Prefer explicit error handling over `unwrap()`

### Code Organization
```rust
// Good example
use anyhow::Result;

pub struct MeritCalculator {
    // fields...
}

impl MeritCalculator {
    /// Creates a new MeritCalculator instance
    pub fn new() -> Self {
        // implementation...
    }
    
    /// Calculates merit points for a contribution
    /// 
    /// # Arguments
    /// * `contribution` - The contribution to evaluate
    /// 
    /// # Returns
    /// The calculated merit points
    pub fn calculate(&self, contribution: &Contribution) -> Result<i32> {
        // implementation...
    }
}
```

### Documentation
- Add doc comments for all public items
- Include examples in doc comments where helpful
- Update README.md for user-facing changes

## 🧪 Testing Requirements

All contributions must include appropriate tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_merit_calculation() {
        let calculator = MeritCalculator::new();
        let contribution = Contribution::new(/* ... */);
        
        let result = calculator.calculate(&contribution).unwrap();
        assert_eq!(result, 50);
    }
}
```

Run tests with:
```bash
cargo test
cargo test -- --nocapture  # To see println! output
```

## 📊 Merit Point Allocation

Your contributions earn merit points automatically:

| Contribution Type | Base Points | Quality Multiplier |
|------------------|-------------|-------------------|
| Bug Fix | 30 | 1.0 - 2.0x |
| New Feature | 40 | 1.0 - 2.5x |
| Documentation | 20 | 1.0 - 1.5x |
| Code Review | 10 | 1.0 - 1.5x |
| Security Fix | 50 | 1.5 - 3.0x |

Quality multipliers are determined by:
- CodeRabbit analysis score
- Test coverage
- Documentation completeness
- Code review feedback

## 🚀 Pull Request Process

1. **Automated Checks**: CodeRabbit will automatically review your PR
2. **Human Review**: Core contributors will review your code
3. **Merit Calculation**: Points are calculated based on contribution type and quality
4. **Role Update**: Your Discord role updates automatically upon merge

### PR Checklist
- [ ] Code follows style guidelines
- [ ] Tests pass locally
- [ ] Documentation is updated
- [ ] Commit messages follow convention
- [ ] PR description is clear and complete

## 🐛 Reporting Issues

When reporting issues, include:
- Clear, descriptive title
- Steps to reproduce
- Expected vs actual behavior
- System information (OS, Rust version, etc.)
- Relevant logs or error messages

## 💬 Communication

- **Discord**: Primary communication channel
- **GitHub Discussions**: Feature requests and ideas
- **Issue Tracker**: Bug reports and task tracking

## 🎖️ Recognition

Contributors are recognized through:
- Automatic Discord role progression
- Mentions in release notes
- Contributor spotlight in Discord
- Special badges for exceptional contributions

## ⚖️ Code of Conduct

- Be respectful and inclusive
- Provide constructive feedback
- Help others learn and grow
- Focus on what's best for the community

## 📚 Resources

- [Rust Book](https://doc.rust-lang.org/book/)
- [Serenity Documentation](https://docs.rs/serenity/)
- [Actix Web Guide](https://actix.rs/docs/)
- [Discord Developer Portal](https://discord.com/developers/docs)

## 🤔 Questions?

- Ask in our Discord `#dev-help` channel
- Open a GitHub Discussion
- Tag `@maintainers` for urgent issues

---

**Remember**: Every contribution matters, and our automated system ensures fair recognition for all contributors. Welcome to our merit-based community! 🚀
