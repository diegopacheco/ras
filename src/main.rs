use pdf_extract::extract_text;
use regex::Regex;
use reqwest::blocking::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::panic;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use once_cell::sync::Lazy;

static SANITIZE_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r#"[<>:"/\\|?*\x00-\x1f]"#).unwrap());
static ID_REGEX: Lazy<Regex> = Lazy::new(|| Regex::new(r"/abs/(\d+\.\d+)").unwrap());

#[derive(Debug, Clone)]
struct Paper {
    id: String,
    title: String,
    pdf_url: String,
}

#[derive(Serialize)]
struct OpenAIRequest {
    model: String,
    messages: Vec<Message>,
    max_completion_tokens: u32,
}

#[derive(Serialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct OpenAIResponse {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ResponseMessage,
}

#[derive(Deserialize)]
struct ResponseMessage {
    content: String,
}

fn print_banner() {
    println!(r#"
  _____            _____
 |  __ \     /\   / ____|
 | |__) |   /  \ | (___
 |  _  /   / /\ \ \___ \
 | | \ \  / ____ \____) |
 |_|  \_\/_/    \_\_____/

        by Diego Pacheco
"#);
}

fn get_ras_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME environment variable not set");
    PathBuf::from(home).join("ras")
}

fn main() {
    print_banner();

    let ras_dir = get_ras_dir();
    let papers_dir = ras_dir.join("papers");
    let summary_dir = ras_dir.join("summary");

    fs::create_dir_all(&papers_dir).expect("Failed to create papers directory");
    fs::create_dir_all(&summary_dir).expect("Failed to create summary directory");

    let existing_summaries = get_existing_summaries(&summary_dir);
    println!("Found {} existing summaries", existing_summaries.len());

    let client = Client::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36")
        .build()
        .expect("Failed to create HTTP client");

    println!("Fetching papers from arXiv...");
    let papers = fetch_arxiv_papers(&client);
    println!("Found {} papers", papers.len());

    let papers_to_process: Vec<Paper> = papers
        .into_iter()
        .filter(|p| !existing_summaries.contains(&sanitize_filename(&p.title)))
        .collect();

    println!("{} papers need processing", papers_to_process.len());

    let openai_key = Arc::new(std::env::var("OPEN_AI_API_KEY").expect("OPEN_AI_API_KEY environment variable not set"));
    let papers_dir = Arc::new(papers_dir);
    let summary_dir = Arc::new(summary_dir);
    let client = Arc::new(client);

    let chunks: Vec<Vec<Paper>> = papers_to_process
        .chunks(10)
        .map(|c| c.to_vec())
        .collect();

    let total_papers = papers_to_process.len();
    let mut processed = 0;

    for chunk in chunks {
        let mut handles = vec![];

        for paper in chunk {
            let openai_key = Arc::clone(&openai_key);
            let papers_dir = Arc::clone(&papers_dir);
            let summary_dir = Arc::clone(&summary_dir);
            let client = Arc::clone(&client);

            let handle = thread::spawn(move || {
                process_paper(&paper, &papers_dir, &summary_dir, &openai_key, &client)
            });
            handles.push(handle);
        }

        for handle in handles {
            let _ = handle.join();
            processed += 1;
            println!("Progress: {}/{}", processed, total_papers);
        }
    }

    println!("\nDone!");
}

fn process_paper(paper: &Paper, papers_dir: &PathBuf, summary_dir: &PathBuf, openai_key: &str, client: &Client) {
    println!("Processing: {}", paper.title);

    let pdf_filename = format!("{}.pdf", sanitize_filename(&paper.title));
    let pdf_path = papers_dir.join(&pdf_filename);

    if !pdf_path.exists() {
        println!("  Downloading PDF: {}", paper.title);
        match download_pdf(&client, &paper.pdf_url, &pdf_path) {
            Ok(_) => println!("  PDF saved: {}", pdf_filename),
            Err(e) => {
                println!("  Failed to download PDF: {}", e);
                return;
            }
        }
    } else {
        println!("  PDF already exists: {}", pdf_filename);
    }

    if let Ok(metadata) = fs::metadata(&pdf_path) {
        if metadata.len() < 1000 {
            println!("  PDF file too small, likely corrupted: {}", pdf_filename);
            let _ = fs::remove_file(&pdf_path);
            return;
        }
    }

    println!("  Extracting text from PDF: {}", paper.title);
    let pdf_text = match extract_text_silent(&pdf_path) {
        Ok(text) => {
            if text.trim().is_empty() {
                println!("  PDF text extraction returned empty content: {}", paper.title);
                return;
            }
            text
        },
        Err(e) => {
            println!("  Failed to extract PDF text: {}", e);
            return;
        }
    };

    println!("  Generating summary: {}", paper.title);
    match generate_summary(&client, openai_key, paper, &pdf_text) {
        Ok(summary) => {
            let summary_filename = format!("{}-summary.md", sanitize_filename(&paper.title));
            let summary_path = summary_dir.join(&summary_filename);
            fs::write(&summary_path, summary).expect("Failed to write summary");
            println!("  Summary saved: {}", summary_filename);
        }
        Err(e) => {
            println!("  Failed to generate summary: {}", e);
        }
    }
}

fn get_existing_summaries(summary_dir: &Path) -> HashSet<String> {
    let mut summaries = HashSet::new();
    if let Ok(entries) = fs::read_dir(summary_dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if name.ends_with("-summary.md") {
                    let paper_name = name.trim_end_matches("-summary.md").to_string();
                    summaries.insert(paper_name);
                }
            }
        }
    }
    summaries
}

fn sanitize_filename(name: &str) -> String {
    let sanitized = SANITIZE_REGEX.replace_all(name, "_").to_string();
    let sanitized = sanitized.trim().to_string();
    if sanitized.chars().count() > 200 {
        sanitized.chars().take(200).collect()
    } else {
        sanitized
    }
}

fn fetch_arxiv_papers(client: &Client) -> Vec<Paper> {
    let mut all_papers = Vec::new();
    let base_url = "https://arxiv.org/list/cs.AI/recent";

    let response = client.get(base_url).send().expect("Failed to fetch arXiv page");
    let html = response.text().expect("Failed to read response");
    let document = Html::parse_document(&html);

    let dt_selector = Selector::parse("dt").unwrap();
    let dd_selector = Selector::parse("dd").unwrap();
    let a_selector = Selector::parse("a").unwrap();
    let title_selector = Selector::parse("div.list-title").unwrap();

    let dts: Vec<_> = document.select(&dt_selector).collect();
    let dds: Vec<_> = document.select(&dd_selector).collect();

    for (dt, dd) in dts.iter().zip(dds.iter()) {
        if all_papers.len() >= 100 {
            break;
        }

        let mut paper_id = String::new();
        for a in dt.select(&a_selector) {
            if let Some(href) = a.value().attr("href") {
                if let Some(caps) = ID_REGEX.captures(href) {
                    paper_id = caps.get(1).unwrap().as_str().to_string();
                    break;
                }
            }
        }

        if paper_id.is_empty() {
            continue;
        }

        let mut title = String::new();
        for div in dd.select(&title_selector) {
            title = div.text().collect::<String>();
            title = title.replace("Title:", "").trim().to_string();
            break;
        }

        if title.is_empty() {
            title = format!("Paper-{}", paper_id);
        }

        all_papers.push(Paper {
            id: paper_id.clone(),
            title,
            pdf_url: format!("https://arxiv.org/pdf/{}.pdf", paper_id),
        });
    }

    if all_papers.len() < 100 {
        let show_url = "https://arxiv.org/list/cs.AI/recent?skip=0&show=100";
        if let Ok(response) = client.get(show_url).send() {
            if let Ok(html) = response.text() {
                let document = Html::parse_document(&html);
                let dts: Vec<_> = document.select(&dt_selector).collect();
                let dds: Vec<_> = document.select(&dd_selector).collect();

                for (dt, dd) in dts.iter().zip(dds.iter()) {
                    if all_papers.len() >= 100 {
                        break;
                    }

                    let mut paper_id = String::new();
                    for a in dt.select(&a_selector) {
                        if let Some(href) = a.value().attr("href") {
                            if let Some(caps) = ID_REGEX.captures(href) {
                                paper_id = caps.get(1).unwrap().as_str().to_string();
                                break;
                            }
                        }
                    }

                    if paper_id.is_empty() {
                        continue;
                    }

                    if all_papers.iter().any(|p| p.id == paper_id) {
                        continue;
                    }

                    let mut title = String::new();
                    for div in dd.select(&title_selector) {
                        title = div.text().collect::<String>();
                        title = title.replace("Title:", "").trim().to_string();
                        break;
                    }

                    if title.is_empty() {
                        title = format!("Paper-{}", paper_id);
                    }

                    all_papers.push(Paper {
                        id: paper_id.clone(),
                        title,
                        pdf_url: format!("https://arxiv.org/pdf/{}.pdf", paper_id),
                    });
                }
            }
        }
    }

    all_papers
}

fn extract_text_silent(path: &Path) -> Result<String, String> {
    let path_buf = path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();

    thread::spawn(move || {
        unsafe {
            let dev_null = fs::File::open("/dev/null").unwrap();
            let dev_null_fd = dev_null.as_raw_fd();
            let stdout_fd = libc::dup(1);
            let stderr_fd = libc::dup(2);
            libc::dup2(dev_null_fd, 1);
            libc::dup2(dev_null_fd, 2);
            drop(dev_null);

            panic::set_hook(Box::new(|_| {}));
            let result = panic::catch_unwind(|| extract_text(&path_buf));
            let _ = panic::take_hook();

            libc::dup2(stdout_fd, 1);
            libc::dup2(stderr_fd, 2);
            libc::close(stdout_fd);
            libc::close(stderr_fd);

            let _ = tx.send(result);
        }
    });

    match rx.recv_timeout(Duration::from_secs(120)) {
        Ok(Ok(Ok(text))) => Ok(text),
        Ok(Ok(Err(e))) => Err(e.to_string()),
        Ok(Err(_)) => Err("PDF extraction panicked".to_string()),
        Err(_) => Err("PDF extraction timed out".to_string()),
    }
}

fn download_pdf(client: &Client, url: &str, path: &Path) -> Result<(), String> {
    let response = client.get(url).send().map_err(|e| e.to_string())?;
    let bytes = response.bytes().map_err(|e| e.to_string())?;
    let mut file = fs::File::create(path).map_err(|e| e.to_string())?;
    file.write_all(&bytes).map_err(|e| e.to_string())?;
    Ok(())
}

fn generate_summary(client: &Client, api_key: &str, paper: &Paper, pdf_text: &str) -> Result<String, String> {
    let truncated_text: String = if pdf_text.chars().count() > 50000 {
        pdf_text.chars().take(50000).collect()
    } else {
        pdf_text.to_string()
    };

    let prompt = format!(
        r#"Please provide a comprehensive, evidence-based summary of the following academic paper based on the provided text.
        Title: {}
        arXiv ID: {}
        PDF URL: {}

        Paper Content:
        {}

        Please analyze the text provided and structure your summary using the following specific sections:
        1. **Overview**: A concise description of the paper's core mission, what it introduces (e.g., specific benchmarks, datasets, or models), and its primary goal.
        2. **Key Results**: detailed quantitative findings. Do not be vague. Extract specific metrics, leaderboard rankings, scores (e.g., "Model X scored 56.1%"), and domain-specific performance comparisons.
        3. **Methodology**: Explain the specific approach used. Detail the dataset composition (e.g., number of test cases, expert sources) and the evaluation/grading process (e.g., "hurdle criteria," "grounding checks," or specific algorithms).
        4. **Critical Insights**: Discuss the nuances, limitations, or specific behaviors observed in the study. Look for failure modes (e.g., hallucinations), performance gaps between domains, or qualitative observations made by the authors.

        **Constraint:** Do not hallucinate. Base the summary *strictly* on the provided text context."#,
        paper.title, paper.id, paper.pdf_url, &truncated_text
    );

    let request = OpenAIRequest {
        model: "gpt-4o-mini".to_string(),
        messages: vec![Message {
            role: "user".to_string(),
            content: prompt,
        }],
        max_completion_tokens: 2000,
    };

    let max_retries = 3;
    let mut last_error = String::new();

    for attempt in 0..max_retries {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(500 * (attempt as u64 + 1)));
        }

        let response = match client
            .post("https://api.openai.com/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&request)
            .send() {
                Ok(r) => r,
                Err(e) => {
                    last_error = e.to_string();
                    continue;
                }
            };

        let status = response.status();
        let body = match response.text() {
            Ok(b) => b,
            Err(e) => {
                last_error = e.to_string();
                continue;
            }
        };

        if status.as_u16() == 429 || status.as_u16() >= 500 {
            last_error = format!("API error {}: {}", status, body);
            continue;
        }

        if !status.is_success() {
            return Err(format!("API error {}: {}", status, body));
        }

        let api_response: OpenAIResponse = match serde_json::from_str(&body) {
            Ok(r) => r,
            Err(e) => {
                last_error = format!("Parse error: {} - Body: {}", e, body);
                continue;
            }
        };

        if api_response.choices.is_empty() {
            return Err("No response from API".to_string());
        }

        let summary_content = &api_response.choices[0].message.content;
        let full_summary = format!(
            "# {}\n\n**arXiv ID**: {}\n**PDF**: {}\n\n---\n\n{}",
            paper.title, paper.id, paper.pdf_url, summary_content
        );
        return Ok(full_summary);
    }

    Err(format!("Failed after {} retries: {}", max_retries, last_error))
}
